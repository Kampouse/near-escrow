"""DEPRECATED — Use the Rust daemon instead.

This Python relayer is superseded by the inlayer daemon's built-in relayer thread.
The Rust version runs inside `inlayer daemon --foreground` when execution_mode="escrow"
and requires no separate process.

    Rust equivalent: near-inlayer/worker/src/daemon/escrow_commands.rs
    CLI: inlayer relayer

Kept for reference and testing only. Do not run alongside the Rust daemon —
they will double-submit actions.

---

Nostr Relayer for NEAR Escrow Marketplace (msig-v2).

Watches for agent task events (kind 41000) on Nostr relays.
When a task event contains a signed action, extracts it and calls
msig.execute() on the agent's multisig contract.

The relayer does NOT create escrows directly — it routes through the
agent's msig, so escrow.agent = msig address (correct identity).

Flow:
  Agent signs CreateEscrow action → embeds in Nostr event → Relayer
  extracts signed action → calls msig.execute() → msig verifies ed25519
  signature → msig calls escrow.create_escrow (escrow.agent = msig)

Also handles generic signed actions (kind 41003) for fund/cancel/withdraw.

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
from collections import OrderedDict
from pathlib import Path

import websockets

try:
    from near_api.account import Account
    from near_api.providers import JsonProvider
    from near_api.signer import Signer
except ImportError:
    print("pip install near-api-py")
    sys.exit(1)

log = logging.getLogger("relayer")

# Nostr event kinds for the escrow marketplace
KIND_TASK = 41000       # Task announcement with signed CreateEscrow action
KIND_CLAIM = 41001      # Worker claims a job
KIND_RESULT = 41002     # Worker submits result
KIND_ACTION = 41003     # Generic signed action (fund, cancel, withdraw, rotate)


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

    # Extract signed actions from tags
    action_json = tags.get("action", [None])[0]
    action_sig_hex = tags.get("action_sig", [None])[0]
    fund_action_json = tags.get("fund_action", [None])[0]
    fund_action_sig_hex = tags.get("fund_action_sig", [None])[0]

    return {
        "event_id": event.get("id"),
        "pubkey": event.get("pubkey"),
        "job_id": tags.get("job_id", [None])[0],
        "reward_amount": reward_parts[0] if len(reward_parts) > 0 else "0",
        "reward_token": reward_parts[1] if len(reward_parts) > 1 else "near",
        "timeout_hours": int(tags.get("timeout", ["24"])[0]),
        "agent": tags.get("agent", [None])[0],           # msig contract address
        "escrow_contract": tags.get("escrow", [""])[0],
        "npub": tags.get("npub", [None])[0],
        "verifier_fee": tags.get("verifier_fee", [None])[0],
        "score_threshold": tags.get("score_threshold", [None])[0],
        "category": tags.get("category", [None])[0],
        "skills": tags.get("skills", []),
        "task_description": content.get("task_description", ""),
        "criteria": content.get("criteria", ""),
        "created_at": event.get("created_at", 0),
        # Signed action fields
        "action_json": action_json,
        "action_sig_hex": action_sig_hex,
        "fund_action_json": fund_action_json,
        "fund_action_sig_hex": fund_action_sig_hex,
    }


class NostrRelayer:
    """Watches Nostr relays for signed actions and submits to msig contracts."""

    def __init__(self, config: dict):
        self.config = config
        self.relays = config.get("relays", [
            "wss://nostr-relay-production.up.railway.app/"
        ])
        self.processed_events: OrderedDict[str, bool] = OrderedDict()
        self.max_processed = config.get("max_processed_cache", 10000)

        # Retry queue for failed actions
        self.retry_queue: list[dict] = []
        self.max_retries = config.get("max_retries", 3)

        # Rate limiting per msig address
        self.rate_limits: dict[str, float] = {}           # msig -> last_submit_time
        self.rate_limit_seconds = config.get("rate_limit_seconds", 10)
        self.rate_limit_counts: dict[str, list[float]] = {}  # msig -> list of timestamps
        self.rate_limit_hourly_max = config.get("rate_limit_hourly_max", 100)

        # NEAR client setup
        rpc_url = config.get("rpc_url", "https://rpc.testnet.near.org")
        relayer_id = config.get("relayer_account_id")

        key_str = os.environ.get("NEAR_RELAYER_KEY", "")
        if not key_str:
            log.warning("NEAR_RELAYER_KEY not set — relayer will watch only")

        if key_str and relayer_id:
            provider = JsonProvider(rpc_url)
            signer = Signer(relayer_id, key_str)
            self.account = Account(provider, signer)
        else:
            self.account = None

    async def watch(self):
        """Connect to relays and subscribe to task events."""
        log.info("Connecting to %d relays", len(self.relays))

        # Start background retry loop
        asyncio.create_task(self._retry_loop())

        since = int(time.time())
        subscription = json.dumps([
            "REQ",
            "escrow-relay",
            {"kinds": [KIND_TASK, KIND_CLAIM, KIND_RESULT, KIND_ACTION], "since": since}
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

    def _check_rate_limit(self, msig_address: str) -> bool:
        """Check rate limit for a given msig address. Returns True if allowed."""
        now = time.time()

        # Check minimum interval between submissions
        last_submit = self.rate_limits.get(msig_address, 0)
        if now - last_submit < self.rate_limit_seconds:
            log.warning(
                "Rate limit: msig=%s too soon (%.1fs < %ds)",
                msig_address, now - last_submit, self.rate_limit_seconds,
            )
            return False

        # Check hourly maximum
        timestamps = self.rate_limit_counts.setdefault(msig_address, [])
        # Prune timestamps older than 1 hour
        cutoff = now - 3600
        self.rate_limit_counts[msig_address] = [t for t in timestamps if t > cutoff]
        timestamps = self.rate_limit_counts[msig_address]

        if len(timestamps) >= self.rate_limit_hourly_max:
            log.warning(
                "Rate limit: msig=%s exceeded hourly max (%d/%d)",
                msig_address, len(timestamps), self.rate_limit_hourly_max,
            )
            return False

        return True

    def _record_rate_limit(self, msig_address: str):
        """Record a submission timestamp for rate limiting."""
        now = time.time()
        self.rate_limits[msig_address] = now
        self.rate_limit_counts.setdefault(msig_address, []).append(now)

    def _verify_event_signature(self, event: dict) -> bool:
        """Verify the Nostr event signature.

        Returns True if valid (or nostr_sdk unavailable — graceful degradation),
        False if signature is invalid.
        """
        try:
            from nostr_sdk import Event
        except ImportError:
            log.debug("nostr_sdk not available — skipping event signature verification")
            return True

        try:
            event_id = event.get("id", "")
            pubkey = event.get("pubkey", "")
            sig = event.get("sig", "")
            if not event_id or not pubkey or not sig:
                log.warning("Event missing id/pubkey/sig — cannot verify")
                return False

            nostr_event = Event(
                id=event_id,
                pubkey=pubkey,
                created_at=event.get("created_at", 0),
                kind=event.get("kind", 0),
                tags=event.get("tags", []),
                content=event.get("content", ""),
                sig=sig,
            )
            if not nostr_event.verify():
                log.warning("Invalid Nostr signature on event %s", event_id[:16])
                return False

            return True
        except Exception as e:
            log.warning("Event signature verification error: %s", e)
            return False

    def _submit_action(self, msig_address: str, action_json: str, sig_bytes: bytes) -> dict:
        """Submit a signed action to the msig contract with receipt verification.

        Returns dict with:
            success: bool — did execute() succeed on-chain?
            tx_hash: str — transaction hash
            nonce_consumed: bool — was the nonce consumed? (false = safe to retry)
            error: str | None — failure reason if not success

        Raises only on network/RPC errors (tx never reached the chain).
        """
        if not self.account:
            raise RuntimeError("No NEAR account configured")

        nonce_before = self._read_nonce(msig_address)

        result = self.account.function_call(
            msig_address,
            "execute",
            json.dumps({
                "action_json": action_json,
                "signature": list(sig_bytes),
            }).encode("utf-8"),
            gas=300_000_000_000_000,  # 300 Tgas
            amount=0,
        )

        # near-api-py returns a dict-like result with status/receipts
        tx_hash = ""
        success = False
        error = None

        if isinstance(result, dict):
            tx_hash = result.get("transaction", {}).get("hash", "")
            status = result.get("status", {})
            if "SuccessValue" in status:
                success = True
            elif "Failure" in status:
                failure = status["Failure"]
                error = json.dumps(failure)
            else:
                error = f"Unexpected status: {json.dumps(status)}"
        else:
            # Some near-api-py versions return an object with .status
            tx_hash = getattr(result, "transaction", {}).get("hash", "") if hasattr(result, "transaction") else ""
            status_obj = getattr(result, "status", {}) if hasattr(result, "status") else {}
            if hasattr(status_obj, "get") and "SuccessValue" in status_obj:
                success = True
            else:
                error = f"Unexpected result type: {type(result).__name__}"

        # Check if nonce was consumed by reading it again
        nonce_consumed = False
        if not success:
            try:
                nonce_after = self._read_nonce(msig_address)
                nonce_consumed = nonce_after != nonce_before
            except Exception:
                # Can't read nonce — assume consumed to be safe
                nonce_consumed = True

        if success:
            log.info("TX confirmed: hash=%s msig=%s", tx_hash[:16], msig_address)
        else:
            log.warning(
                "TX failed on-chain: hash=%s msig=%s nonce_consumed=%s error=%s",
                tx_hash[:16], msig_address, nonce_consumed, (error or "")[:120],
            )

        return {
            "success": success,
            "tx_hash": tx_hash,
            "nonce_consumed": nonce_consumed,
            "error": error,
        }

    def _read_nonce(self, msig_address: str) -> int:
        """Read current nonce from msig contract via view call."""
        result = self.account.provider.view_call(
            msig_address,
            "get_nonce",
            b"",
        )
        if result.get("result"):
            data = bytes(result["result"]).decode()
            return int(data)
        return 0

    def _enqueue_retry(self, msig_address: str, action_json: str,
                       sig_bytes: bytes, action_type: str, attempt: int = 1):
        """Enqueue a failed action for retry."""
        if attempt > self.max_retries:
            log.error(
                "Dropping action after %d retries: msig=%s type=%s",
                attempt, msig_address, action_type,
            )
            return

        self.retry_queue.append({
            "msig_address": msig_address,
            "action_json": action_json,
            "sig_bytes": sig_bytes,
            "action_type": action_type,
            "attempt": attempt,
            "enqueued_at": time.time(),
        })
        log.info(
            "Enqueued for retry (attempt %d/%d): msig=%s type=%s",
            attempt, self.max_retries, msig_address, action_type,
        )

    async def _retry_loop(self):
        """Background loop that retries failed actions every 30 seconds."""
        while True:
            await asyncio.sleep(30)
            if not self.retry_queue:
                continue

            log.info("Retry queue: %d items pending", len(self.retry_queue))
            still_queued = []

            for item in self.retry_queue:
                msig = item["msig_address"]

                # Respect rate limits
                if not self._check_rate_limit(msig):
                    still_queued.append(item)
                    continue

                try:
                    result = self._submit_action(msig, item["action_json"], item["sig_bytes"])
                    self._record_rate_limit(msig)

                    if result["success"]:
                        log.info(
                            "Retry succeeded: msig=%s type=%s attempt=%d tx=%s",
                            msig, item["action_type"], item["attempt"], result["tx_hash"][:16],
                        )
                    elif result["nonce_consumed"]:
                        log.error(
                            "Retry failed, nonce consumed — dropping: msig=%s type=%s attempt=%d",
                            msig, item["action_type"], item["attempt"],
                        )
                        # Don't re-enqueue — nonce is gone, this action is dead
                    else:
                        # Nonce not consumed — safe to retry
                        self._enqueue_retry(
                            msig, item["action_json"], item["sig_bytes"],
                            item["action_type"], item["attempt"] + 1,
                        )
                except Exception as e:
                    log.warning(
                        "Retry RPC error: msig=%s type=%s attempt=%d: %s",
                        msig, item["action_type"], item["attempt"], e,
                    )
                    self._enqueue_retry(
                        msig, item["action_json"], item["sig_bytes"],
                        item["action_type"], item["attempt"] + 1,
                    )

            self.retry_queue = still_queued

    async def _handle_event(self, event: dict):
        """Route events by kind."""
        kind = event.get("kind")
        event_id = event.get("id", "")

        if event_id in self.processed_events:
            return

        # Verify Nostr event signature if nostr_sdk available
        if not self._verify_event_signature(event):
            log.warning("Dropping event %s — Nostr signature verification failed", event_id[:16])
            return

        self.processed_events[event_id] = True
        # Evict oldest entries when over limit (FIFO)
        while len(self.processed_events) > self.max_processed:
            self.processed_events.popitem(last=False)

        if kind == KIND_TASK:
            task = parse_task_event(event)
            if task and task["job_id"] and task["action_json"]:
                await self._on_signed_action(task)
            elif task and task["job_id"]:
                log.warning(
                    "Task %s has no signed action — skipping (needs msig flow)",
                    task["job_id"],
                )
        elif kind == KIND_ACTION:
            await self._on_generic_action(event)
        elif kind == KIND_CLAIM:
            log.info(
                "Claim event: job=%s worker=%s",
                event.get("tags", [[]])[0][1] if event.get("tags") else "?",
                event.get("tags", [[], []])[1][1] if len(event.get("tags", [])) > 1 else "?",
            )
        elif kind == KIND_RESULT:
            log.info(
                "Result event: job=%s",
                event.get("tags", [[]])[0][1] if event.get("tags") else "?",
            )

    async def _on_signed_action(self, task: dict):
        """Submit signed CreateEscrow + FundEscrow actions to the agent's msig contract.

        Flow for kind 41000:
        1. Submit CreateEscrow action (action + action_sig tags)
        2. Submit FundEscrow action (fund_action + fund_action_sig tags) if present
        3. Publish kind 41004 (FUNDED) to Nostr on success
        """
        log.info(
            "Signed action: job_id=%s msig=%s",
            task["job_id"], task["agent"],
        )

        if not self.account:
            log.warning("No NEAR account — skipping action submission")
            return

        msig_address = task["agent"]
        if not msig_address:
            log.error("No msig address (agent tag) in event")
            return

        # --- Step 1: Submit CreateEscrow ---
        if not task["action_sig_hex"]:
            log.error("No action signature in event for job %s", task["job_id"])
            return

        try:
            sig_bytes = bytes.fromhex(task["action_sig_hex"])
        except (ValueError, TypeError):
            log.error("Invalid signature hex for job %s", task["job_id"])
            return

        if len(sig_bytes) != 64:
            log.error("Invalid signature length %d for job %s (expected 64)", len(sig_bytes), task["job_id"])
            return

        # Rate limit check
        if not self._check_rate_limit(msig_address):
            log.warning("Rate limited, enqueueing signed action for retry: job %s", task["job_id"])
            self._enqueue_retry(
                msig_address, task["action_json"], sig_bytes,
                "CreateEscrow", attempt=1,
            )
            return

        try:
            result = self._submit_action(msig_address, task["action_json"], sig_bytes)
            self._record_rate_limit(msig_address)

            if result["success"]:
                log.info("CreateEscrow confirmed: job_id=%s tx=%s", task["job_id"], result["tx_hash"][:16])
            elif result["nonce_consumed"]:
                log.error(
                    "CreateEscrow failed (nonce consumed) — dropping: job=%s error=%s",
                    task["job_id"], (result["error"] or "")[:80],
                )
                return  # Can't continue — nonce is ahead
            else:
                log.warning("CreateEscrow reverted — retrying: job=%s", task["job_id"])
                self._enqueue_retry(
                    msig_address, task["action_json"], sig_bytes,
                    "CreateEscrow", attempt=1,
                )
                return

        except Exception as e:
            log.error("RPC error on CreateEscrow for %s: %s", task["job_id"], e)
            self._enqueue_retry(
                msig_address, task["action_json"], sig_bytes,
                "CreateEscrow", attempt=1,
            )
            return

        # --- Step 2: Submit FundEscrow (if present) ---
        if task.get("fund_action_json") and task.get("fund_action_sig_hex"):
            try:
                fund_sig_bytes = bytes.fromhex(task["fund_action_sig_hex"])
            except (ValueError, TypeError):
                log.error("Invalid fund_action signature hex for job %s", task["job_id"])
                return

            if len(fund_sig_bytes) != 64:
                log.error("Invalid fund_action signature length for job %s", task["job_id"])
                return

            # Rate limit check
            if not self._check_rate_limit(msig_address):
                log.warning("Rate limited, enqueueing FundEscrow for retry: job %s", task["job_id"])
                self._enqueue_retry(
                    msig_address, task["fund_action_json"], fund_sig_bytes,
                    "FundEscrow", attempt=1,
                )
                return

            try:
                fund_result = self._submit_action(msig_address, task["fund_action_json"], fund_sig_bytes)
                self._record_rate_limit(msig_address)

                if fund_result["success"]:
                    log.info("FundEscrow confirmed: job_id=%s tx=%s", task["job_id"], fund_result["tx_hash"][:16])

                    # --- Step 3: Publish kind 41004 (FUNDED) ---
                    await self._publish_funded_event(task)

                elif fund_result["nonce_consumed"]:
                    log.error(
                        "FundEscrow failed (nonce consumed) — escrow created but unfunded: job=%s error=%s",
                        task["job_id"], (fund_result["error"] or "")[:80],
                    )
                else:
                    log.warning("FundEscrow reverted — retrying: job=%s", task["job_id"])
                    self._enqueue_retry(
                        msig_address, task["fund_action_json"], fund_sig_bytes,
                        "FundEscrow", attempt=1,
                    )

            except Exception as e:
                log.error("RPC error on FundEscrow for %s: %s", task["job_id"], e)
                self._enqueue_retry(
                    msig_address, task["fund_action_json"], fund_sig_bytes,
                    "FundEscrow", attempt=1,
                )
        else:
            # No fund_action in event — just log (may come via separate 41003)
            log.info("No fund_action in event for job %s — awaiting separate funding", task["job_id"])

    async def _publish_funded_event(self, task: dict):
        """Publish a kind 41004 (FUNDED) event to Nostr relays."""
        import hashlib
        import time as _time

        event = {
            "kind": 41004,
            "created_at": int(_time.time()),
            "tags": [
                ["job_id", task["job_id"]],
                ["agent", task["agent"]],
                ["escrow", task["escrow_contract"]],
                ["reward", task["reward_amount"], task["reward_token"]],
            ],
            "content": json.dumps({"status": "funded"}),
        }

        # Post to all relays (best-effort, unsigned for now)
        message = json.dumps(["EVENT", event])
        for relay_url in self.relays:
            try:
                async with websockets.connect(relay_url) as ws:
                    await ws.send(message)
                    resp = await asyncio.wait_for(ws.recv(), timeout=5)
                    log.info("Published 41004 FUNDED for %s to %s: %s", task["job_id"], relay_url, resp[:80])
            except Exception as e:
                log.warning("Failed to publish 41004 to %s: %s", relay_url, e)

    async def _on_generic_action(self, event: dict):
        """Handle kind 41003 generic signed action events (fund, cancel, withdraw, etc.)."""
        tags = {t[0]: t[1:] for t in event.get("tags", []) if len(t) >= 2}

        msig_address = tags.get("agent", [None])[0]
        action_json = tags.get("action", [None])[0]
        action_sig_hex = tags.get("action_sig", [None])[0]

        if not all([msig_address, action_json, action_sig_hex]):
            log.warning("Incomplete action event %s", event.get("id", "?"))
            return

        if not self.account:
            log.warning("No NEAR account — skipping action")
            return

        try:
            sig_bytes = bytes.fromhex(action_sig_hex)
        except (ValueError, TypeError):
            log.error("Invalid signature hex in action event")
            return

        if len(sig_bytes) != 64:
            log.error("Invalid signature length %d (expected 64)", len(sig_bytes))
            return

        # Rate limit check — enqueue instead of dropping
        if not self._check_rate_limit(msig_address):
            log.warning("Rate limited, enqueueing generic action for retry: msig %s", msig_address)
            self._enqueue_retry(
                msig_address, action_json, sig_bytes,
                "generic", attempt=1,
            )
            return

        action_type = "unknown"
        try:
            parsed = json.loads(action_json)
            action_type = parsed.get("action", {}).get("type", "unknown")
        except json.JSONDecodeError:
            pass

        log.info("Generic action: type=%s msig=%s", action_type, msig_address)

        try:
            result = self._submit_action(msig_address, action_json, sig_bytes)
            self._record_rate_limit(msig_address)

            if result["success"]:
                log.info("Action confirmed on-chain: type=%s tx=%s", action_type, result["tx_hash"][:16])
            elif result["nonce_consumed"]:
                log.error(
                    "Action failed but nonce consumed — dropping: type=%s tx=%s error=%s",
                    action_type, result["tx_hash"][:16], (result["error"] or "")[:80],
                )
            else:
                log.warning("Action reverted (nonce not consumed) — retrying: type=%s", action_type)
                self._enqueue_retry(
                    msig_address, action_json, sig_bytes,
                    action_type, attempt=1,
                )

        except Exception as e:
            log.error("RPC error executing action: %s", e)
            self._enqueue_retry(
                msig_address, action_json, sig_bytes,
                action_type, attempt=1,
            )


def main():
    parser = argparse.ArgumentParser(description="NEAR Escrow Nostr Relayer (msig-v2)")
    parser.add_argument("--config", "-c", default="config.json")
    parser.add_argument("--dry-run", action="store_true", help="Watch only, don't submit actions")
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
