"""Nostr Relayer for NEAR Escrow Marketplace.

Watches for agent task events (kind 41000) on Nostr relays.
When a task is detected, calls create_escrow() on the NEAR contract
on behalf of the agent (requires agent's pre-authorized key or relayer key).

Also watches for claim events (kind 41001) and result events (kind 41002)
to index on FastNear for discoverability.

Usage:
    export NEAR_RELAYER_KEY=ed25519:...
    python relayer.py --config config.json
"""

import argparse
import asyncio
import json
import logging
import os
import sys
import time
from pathlib import Path

import websockets

try:
    from near_api.account import Account
    from near_api.providers import JsonProvider
    from near_api.signer import Signer
except ImportError:
    print("pip install near-api-py")
    sys.exit(1)

try:
    from nostr_sdk import Client, EventBuilder, Keys, Kind, Filter, Timestamp
except ImportError:
    # Fallback to raw websocket implementation
    Client = None

log = logging.getLogger("relayer")

# Nostr event kinds for the escrow marketplace
KIND_TASK = 41000
KIND_CLAIM = 41001
KIND_RESULT = 41002


def load_config(path: str) -> dict:
    with open(path) as f:
        return json.load(f)


def parse_task_event(event: dict) -> dict | None:
    """Parse a kind 41000 Nostr event into a task dict."""
    if event.get("kind") != KIND_TASK:
        return None

    tags = {t[0]: t[1:] for t in event.get("tags", []) if len(t) >= 2}

    content = {}
    try:
        content = json.loads(event.get("content", "{}"))
    except json.JSONDecodeError:
        log.warning("Invalid JSON content in event %s", event.get("id", "?"))
        return None

    reward_parts = tags.get("reward", ["0", "near"])
    return {
        "event_id": event.get("id"),
        "pubkey": event.get("pubkey"),
        "job_id": tags.get("job_id", [None])[0],
        "reward_amount": reward_parts[0] if len(reward_parts) > 0 else "0",
        "reward_token": reward_parts[1] if len(reward_parts) > 1 else "near",
        "timeout_hours": int(tags.get("timeout", ["24"])[0]),
        "agent": tags.get("agent", [""])[0],
        "escrow_contract": tags.get("escrow", [""])[0],
        "verifier_fee": tags.get("verifier_fee", [None])[0],
        "score_threshold": tags.get("score_threshold", [None])[0],
        "category": tags.get("category", [None])[0],
        "skills": tags.get("skills", []),
        "task_description": content.get("task_description", ""),
        "criteria": content.get("criteria", ""),
        "created_at": event.get("created_at", 0),
    }


class NostrRelayer:
    """Watches Nostr relays for task events and creates on-chain escrows."""

    def __init__(self, config: dict):
        self.config = config
        self.relays = config.get("relays", [
            "wss://nostr-relay-production.up.railway.app/"
        ])
        self.escrow_contract = config.get("escrow_contract")
        self.processed_events: set[str] = set()
        self.max_processed = config.get("max_processed_cache", 10000)

        # NEAR client setup
        rpc_url = config.get("rpc_url", "https://rpc.testnet.near.org")
        relayer_id = config.get("relayer_account_id")

        key_str = os.environ.get("NEAR_RELAYER_KEY", "")
        if not key_str:
            log.warning("NEAR_RELAYER_KEY not set — relayer will watch only, not create escrows")

        if key_str and relayer_id:
            provider = JsonProvider(rpc_url)
            signer = Signer(relayer_id, key_str)
            self.account = Account(provider, signer)
        else:
            self.account = None

    async def watch(self):
        """Connect to relays and subscribe to task events."""
        log.info("Connecting to %d relays", len(self.relays))

        # Subscribe to task events since now
        since = int(time.time())
        subscription = json.dumps([
            "REQ",
            "escrow-relay",
            {"kinds": [KIND_TASK, KIND_CLAIM, KIND_RESULT], "since": since}
        ])

        while True:
            try:
                for relay_url in self.relays:
                    async for ws in websockets.connect(relay_url):
                        await ws.send(subscription)
                        log.info("Subscribed to %s", relay_url)

                        async for raw in ws:
                            msg = json.loads(raw)
                            if msg[0] == "EVENT":
                                event = msg[1]
                                await self._handle_event(event)

            except websockets.ConnectionClosed:
                log.warning("Connection closed, reconnecting in 5s...")
                await asyncio.sleep(5)
            except Exception as e:
                log.error("Relay error: %s", e, exc_info=True)
                await asyncio.sleep(5)

    async def _handle_event(self, event: dict):
        """Route events by kind."""
        kind = event.get("kind")
        event_id = event.get("id", "")

        if event_id in self.processed_events:
            return

        self.processed_events.add(event_id)
        if len(self.processed_events) > self.max_processed:
            # Trim oldest entries
            self.processed_events = set(list(self.processed_events)[-self.max_processed:])

        if kind == KIND_TASK:
            task = parse_task_event(event)
            if task and task["job_id"]:
                await self._on_task(task)
        elif kind == KIND_CLAIM:
            log.info("Claim event: job=%s worker=%s",
                     event.get("tags", [[]])[0][1] if event.get("tags") else "?",
                     event.get("tags", [[], []])[1][1] if len(event.get("tags", [])) > 1 else "?")
        elif kind == KIND_RESULT:
            log.info("Result event: job=%s",
                     event.get("tags", [[]])[0][1] if event.get("tags") else "?")

    async def _on_task(self, task: dict):
        """Create on-chain escrow when a task event is detected."""
        log.info(
            "New task: job_id=%s agent=%s reward=%s %s",
            task["job_id"], task["agent"], task["reward_amount"], task["reward_token"]
        )

        if not self.account:
            log.warning("No NEAR account configured — skipping escrow creation")
            return

        if not self.escrow_contract:
            log.error("No escrow_contract in config")
            return

        try:
            # Create escrow on-chain
            args = json.dumps({
                "job_id": task["job_id"],
                "amount": task["reward_amount"],
                "token": task["reward_token"],
                "timeout_hours": task["timeout_hours"],
                "task_description": task["task_description"],
                "criteria": task["criteria"],
                "verifier_fee": task["verifier_fee"],
                "score_threshold": task["score_threshold"],
            }).encode("utf-8")

            # Attach 1 NEAR storage deposit
            result = self.account.function_call(
                self.escrow_contract,
                "create_escrow",
                args,
                gas=300_000_000_000_000,  # 300 Tgas
                amount=1_000_000_000_000_000_000_000_000,  # 1 NEAR
            )
            log.info("Escrow created: job_id=%s tx=%s", task["job_id"], result)

        except Exception as e:
            log.error("Failed to create escrow for %s: %s", task["job_id"], e)


def main():
    parser = argparse.ArgumentParser(description="NEAR Escrow Nostr Relayer")
    parser.add_argument("--config", "-c", default="config.json")
    parser.add_argument("--dry-run", action="store_true", help="Watch only, don't create escrows")
    args = parser.parse_args()

    config_path = args.config
    if not Path(config_path).exists():
        log.error("Config not found: %s", config_path)
        sys.exit(1)

    config = load_config(config_path)

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s [%(name)s] %(levelname)s: %(message)s",
        datefmt="%H:%M:%S",
    )

    relayer = NostrRelayer(config)

    if args.dry_run:
        relayer.account = None
        log.info("Dry run mode — watching only")

    asyncio.run(relayer.watch())


if __name__ == "__main__":
    main()
