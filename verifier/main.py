"""LLM Verifier Service for NEAR Escrow.

Watches for escrows entering Verifying state, scores the work
using Gemini with multi-pass verification, and delivers the verdict
via promise_yield_resume.

Usage:
    export GEMINI_API_KEY=...
    export NEAR_VERIFIER_KEY=ed25519:...  # Verifier account private key
    python main.py --config config.json
"""

import argparse
import json
import logging
import os
import sys
import time
from pathlib import Path

from near_client import NearClient
from scorer import Scorer

# near-api-py imports
from near_api.account import Account
from near_api.providers import JsonProvider
from near_api.signer import KeyPair, Signer

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(name)s] %(levelname)s: %(message)s",
    datefmt="%H:%M:%S",
)
log = logging.getLogger("verifier")


def load_config(path: str) -> dict:
    """Load config from JSON file."""
    with open(path) as f:
        return json.load(f)


def create_near_client(config: dict) -> NearClient:
    """Create a NearClient from config + env vars."""
    rpc_url = config.get("rpc_url", "https://rpc.testnet.near.org")
    verifier_id = config.get("verifier_account_id")

    # Key from env: NEAR_VERIFIER_KEY=ed25519:base64privatekey
    key_str = os.environ.get("NEAR_VERIFIER_KEY", "")
    if not key_str:
        log.error("NEAR_VERIFIER_KEY env var required (ed25519:base64privatekey)")
        sys.exit(1)

    provider = JsonProvider(rpc_url)
    key_pair = KeyPair(key_str)
    signer = Signer(verifier_id, key_pair)
    account = Account(provider, signer)

    return NearClient(config, account)


def process_verifying_escrow(
    job_id: str,
    data_id_hex: str,
    near: NearClient,
    scorer: Scorer,
) -> bool:
    """Process a single verifying escrow. Returns True on success."""
    # Fetch full escrow state for task/criteria
    escrow = near.get_escrow(job_id)
    if not escrow:
        log.error("Escrow %s not found", job_id)
        return False

    if escrow.get("status") != "Verifying":
        log.warning("Escrow %s is %s, not Verifying — skipping", job_id, escrow.get("status"))
        return False

    result = escrow.get("result")
    if not result:
        log.error("Escrow %s has no result", job_id)
        return False

    task_description = escrow.get("task_description", "")
    criteria = escrow.get("criteria", "")
    threshold = escrow.get("score_threshold", 80)

    log.info(
        "Scoring escrow %s (threshold=%d, criteria=%s...)",
        job_id, threshold, criteria[:60],
    )

    # Score the work
    verdict = scorer.score(
        task_description=task_description,
        criteria=criteria,
        result=result,
        threshold=threshold,
    )

    log.info(
        "Verdict for %s: score=%d, passed=%s",
        job_id, verdict["score"], verdict["passed"],
    )

    # Deliver verdict via promise_yield_resume
    payload = json.dumps({
        "score": verdict["score"],
        "passed": verdict["passed"],
        "detail": verdict["detail"],
    })

    try:
        tx_hash = near.resume_yield(data_id_hex, payload)
        log.info("Resume tx sent for %s: %s", job_id, tx_hash)
        return True
    except Exception as e:
        log.error("Failed to resume yield for %s: %s", job_id, e)
        return False


def main():
    parser = argparse.ArgumentParser(description="NEAR Escrow LLM Verifier")
    parser.add_argument(
        "--config", "-c",
        default="config.json",
        help="Path to config JSON file",
    )
    parser.add_argument(
        "--once",
        action="store_true",
        help="Process pending verifications once, then exit",
    )
    args = parser.parse_args()

    config_path = args.config
    if not Path(config_path).exists():
        # Try example config
        if Path("config.example.json").exists():
            log.error("Config not found. Copy config.example.json to config.json and edit it.")
        else:
            log.error("Config file not found: %s", config_path)
        sys.exit(1)

    config = load_config(config_path)
    poll_interval = config.get("poll_interval_seconds", 3)

    near = create_near_client(config)
    scorer = Scorer(config)

    log.info(
        "Verifier started — watching contract %s on %s",
        config["contract_id"],
        config.get("rpc_url", "testnet"),
    )
    log.info("Model: %s, passes: %d", scorer.model, scorer.passes)

    # Track processed job IDs to avoid re-processing
    # Bounded: trim oldest when exceeding 10k entries
    processed: set[str] = set()
    MAX_PROCESSED = 10_000

    while True:
        try:
            # Poll for verifying escrows directly from contract state
            verifying = near.get_verifying_escrows()

            if not verifying:
                if args.once:
                    log.info("No pending verifications")
                    break
                time.sleep(poll_interval)
                continue

            for escrow in verifying:
                job_id = escrow.get("job_id")
                data_id = escrow.get("data_id")

                if not job_id or not data_id:
                    log.warning("Malformed verifying entry: %s", escrow)
                    continue

                if job_id in processed:
                    continue

                log.info("Found verifying escrow: %s (data_id=%s)", job_id, data_id)

                success = process_verifying_escrow(job_id, data_id, near, scorer)

                if success:
                    processed.add(job_id)
                    if len(processed) > MAX_PROCESSED:
                        # Evict oldest half
                        evict_count = len(processed) // 2
                        for _ in range(evict_count):
                            processed.discard(next(iter(processed)))

            if args.once:
                break

            time.sleep(poll_interval)

        except KeyboardInterrupt:
            log.info("Shutting down")
            break
        except Exception as e:
            log.error("Main loop error: %s", e, exc_info=True)
            time.sleep(poll_interval)


if __name__ == "__main__":
    main()
