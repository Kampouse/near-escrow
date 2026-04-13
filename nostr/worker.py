"""Worker Agent for NEAR Escrow Marketplace (msig-v2 compatible).

Subscribes to Nostr task events (kind 41000), evaluates if the task
matches capabilities, claims on-chain, executes the task, submits result.

In msig-v2 architecture, the worker interacts with the escrow contract
directly — only the agent goes through the msig. The worker:
  - Watches for kind 41000 events on Nostr
  - Waits for escrow to reach Open status (retries if not yet created/funded)
  - Claims directly on escrow (escrow.worker = worker's NEAR account)
  - Executes task
  - Submits result directly on escrow

Usage:
    export NEAR_WORKER_KEY=ed25519:...
    python worker.py --config config.json
"""

import argparse
import asyncio
import json
import logging
import os
import signal
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
    print("pip install near-api-py websockets")
    sys.exit(1)

log = logging.getLogger("worker")

KIND_TASK = 41000
KIND_CLAIM = 41001
KIND_RESULT = 41002

# How long to wait between retries when escrow isn't ready yet
POLL_INTERVAL_SECONDS = 10
# Maximum time to wait for escrow to become Open (seconds)
MAX_WAIT_SECONDS = 600  # 10 minutes


def load_config(path: str) -> dict:
    with open(path) as f:
        return json.load(f)


def parse_multi_tags(tags_list: list, key: str) -> list:
    """Extract all values for a given tag key, handling multi-value tags.

    Nostr allows multiple tags with the same name. The naive dict approach
    (tags = {t[0]: t[1:] ...}) only keeps the LAST one. This collects ALL.
    """
    values = []
    for t in tags_list:
        if len(t) >= 2 and t[0] == key:
            values.extend(t[1:])
    return values


class NostrPoster:
    """Best-effort Nostr event posting for worker lifecycle events.

    Maintains a persistent Nostr client that connects once and reuses
    the connection for all events, reconnecting on failure.
    """

    def __init__(self, relays: list[str], nostr_key_hex: str | None):
        self.relays = relays
        self.nostr_key_hex = nostr_key_hex
        self._client = None
        self._keys = None
        self._connected = False

        if nostr_key_hex:
            try:
                from nostr_sdk import Client, Keys
                self._keys = Keys.from_hex_privkey(nostr_key_hex)
                self._client = Client()
                log.info("Nostr posting enabled (pubkey: %s...)", nostr_key_hex[:16])
            except ImportError:
                log.warning("nostr-sdk not installed — worker events won't be posted")
            except Exception as e:
                log.warning("Failed to init Nostr keys: %s — events won't be posted", e)

    async def _ensure_connected(self) -> bool:
        """Connect once, reuse for all subsequent events."""
        if not self._client or not self._keys:
            return False
        if self._connected:
            return True
        try:
            for relay in self.relays:
                await self._client.add_relay(relay)
            await self._client.connect()
            self._connected = True
            return True
        except Exception as e:
            log.debug("Nostr connect failed: %s", e)
            self._connected = False
            return False

    async def post_event(self, kind: int, tags: list[list[str]], content: str = ""):
        """Post a Nostr event. Best-effort — logs warning on failure, never raises."""
        if not self._client or not self._keys:
            return

        try:
            from nostr_sdk import EventBuilder, Kind, Tag

            if not await self._ensure_connected():
                return

            builder = EventBuilder(kind=Kind(kind), content=content)
            for tag in tags:
                builder = builder.tag(Tag.custom(tag))

            event = builder.to_event(self._keys)
            await self._client.send_event(event)
            log.info("Posted Nostr event: kind=%d", kind)
        except Exception as e:
            log.warning("Failed to post Nostr event (kind=%d): %s", kind, e)
            # Reset connection state so next attempt reconnects
            self._connected = False
            try:
                self._client = type(self._client)()
            except Exception:
                pass


class WorkerAgent:
    """Nostr-based worker agent that claims and executes tasks."""

    def __init__(self, config: dict):
        self.config = config
        self._shutdown = False
        self.relays = config.get("relays", [
            "wss://nostr-relay-production.up.railway.app/"
        ])
        self.escrow_contract = config.get("escrow_contract")

        # Read worker-specific config from nested "worker" section (preferred)
        # or top-level (backward compat)
        worker_config = config.get("worker", {})
        self.capabilities = worker_config.get("capabilities", config.get("capabilities", []))
        self.max_reward = worker_config.get("max_reward", config.get("max_reward", "0"))
        self.processed_jobs: OrderedDict[str, bool] = OrderedDict()
        self.max_processed = config.get("max_processed_cache", 10000)

        # NEAR client
        rpc_url = config.get("rpc_url", "https://rpc.testnet.near.org")
        worker_id = worker_config.get("worker_account_id", config.get("worker_account_id"))

        key_str = os.environ.get("NEAR_WORKER_KEY", "")
        if not key_str or not worker_id:
            log.error("NEAR_WORKER_KEY and worker_account_id required")
            sys.exit(1)

        provider = JsonProvider(rpc_url)
        signer = Signer(worker_id, key_str)
        self.account = Account(provider, signer)
        self.worker_id = worker_id

        # Nostr posting (optional — for broadcasting claim/result events)
        nostr_key = os.environ.get("NOSTR_WORKER_KEY", None) or worker_config.get("nostr_key", None)
        self.nostr_poster = NostrPoster(self.relays, nostr_key)

    def _signal_handler(self, signum, frame):
        """Handle SIGINT/SIGTERM for graceful shutdown."""
        sig_name = signal.Signals(signum).name
        log.info("Received %s — shutting down gracefully...", sig_name)
        self._shutdown = True

    def _verify_event_signature(self, event: dict, expected_npub: str) -> bool:
        """Verify the Nostr event signature against the expected npub.

        Returns True if signature is valid (or nostr_sdk unavailable — graceful
        degradation), False if pubkey mismatch or invalid signature.
        """
        try:
            from nostr_sdk import Event, PublicKey
        except ImportError:
            log.debug("nostr_sdk not available — skipping signature verification")
            return True

        try:
            event_pubkey = event.get("pubkey", "")
            if not event_pubkey:
                log.warning("Event has no pubkey — verification failed")
                return False

            if event_pubkey != expected_npub:
                log.warning(
                    "Event pubkey %s does not match expected npub %s",
                    event_pubkey[:16], expected_npub[:16],
                )
                return False

            # Build a nostr_sdk Event from the raw dict and verify signature
            nostr_event = Event(
                id=event.get("id", ""),
                pubkey=event_pubkey,
                created_at=event.get("created_at", 0),
                kind=event.get("kind", 0),
                tags=event.get("tags", []),
                content=event.get("content", ""),
                sig=event.get("sig", ""),
            )
            if not nostr_event.verify():
                log.warning("Invalid Nostr signature on event %s", event.get("id", "")[:16])
                return False

            return True
        except Exception as e:
            log.warning("Event signature verification error: %s", e)
            return False

    def matches_capabilities(self, task: dict) -> bool:
        """Check if task matches worker's capabilities."""
        if not self.capabilities:
            return True  # Accept everything if no filter set

        task_skills = set(task.get("skills", []))
        task_category = task.get("category", "")

        my_caps = set(self.capabilities)

        # Match if any capability overlaps with task skills or category
        if task_category and task_category in my_caps:
            return True
        if task_skills & my_caps:
            return True

        return False

    async def watch(self):
        """Subscribe to task events and process them."""
        since = int(time.time())
        subscription = json.dumps([
            "REQ",
            "worker-agent",
            {"kinds": [KIND_TASK], "since": since}
        ])

        log.info("Worker %s watching for tasks (capabilities: %s)",
                 self.worker_id, self.capabilities or "all")

        while True:
            if self._shutdown:
                log.info("Shutdown requested — exiting watch loop")
                break
            try:
                for relay_url in self.relays:
                    async for ws in websockets.connect(relay_url):
                        await ws.send(subscription)
                        log.info("Connected to %s", relay_url)

                        async for raw in ws:
                            msg = json.loads(raw)
                            if msg[0] == "EVENT":
                                event = msg[1]
                                await self._handle_task_event(event)

            except websockets.ConnectionClosed:
                log.warning("Connection closed, reconnecting in 5s...")
                await asyncio.sleep(5)
            except Exception as e:
                log.error("Worker error: %s", e, exc_info=True)
                await asyncio.sleep(5)

    async def _handle_task_event(self, event: dict):
        """Process a task event — evaluate, claim, execute, submit."""
        if event.get("kind") != KIND_TASK:
            return

        raw_tags = event.get("tags", [])
        tags = {t[0]: t[1:] for t in raw_tags if len(t) >= 2}

        content = {}
        try:
            content = json.loads(event.get("content", "{}"))
        except json.JSONDecodeError:
            return

        reward_parts = tags.get("reward", ["0"])
        job_id = tags.get("job_id", [None])[0]
        if not job_id or job_id in self.processed_jobs:
            return

        # Use multi-value tag parser for skills
        skills = parse_multi_tags(raw_tags, "skills")

        # Extract npub from tags and verify event signature
        npub = tags.get("npub", [None])[0]
        if npub and not self._verify_event_signature(event, npub):
            log.warning("Event signature verification failed for job %s — skipping", job_id)
            return

        task = {
            "job_id": job_id,
            "reward_amount": reward_parts[0] if reward_parts else "0",
            "timeout_hours": int(tags.get("timeout", ["24"])[0]),
            "escrow_contract": tags.get("escrow", [self.escrow_contract])[0],
            "category": tags.get("category", [None])[0],
            "skills": skills,
            "task_description": content.get("task_description", ""),
            "criteria": content.get("criteria", ""),
        }

        # Check if we can do this task
        if not self.matches_capabilities(task):
            log.debug("Skipping %s — doesn't match capabilities", job_id)
            return

        # Check reward against max_reward
        try:
            reward = int(task["reward_amount"])
            max_r = int(self.max_reward)
            if max_r > 0 and reward > max_r:
                log.debug("Skipping %s — reward %s exceeds max %s", job_id, reward, max_r)
                return
        except (ValueError, TypeError):
            pass  # Can't parse reward — proceed anyway

        log.info("Task matched: %s — %s", job_id, task["task_description"][:80])

        # 1. Wait for escrow to be Open (retries with polling)
        escrow = await self._wait_for_open(task["escrow_contract"], job_id)
        if not escrow:
            log.warning("Escrow %s not open after polling — skipping", job_id)
            return

        # 2. Claim
        try:
            self._claim(task["escrow_contract"], job_id)
            log.info("Claimed job %s", job_id)
        except Exception as e:
            log.error("Failed to claim %s: %s", job_id, e)
            return

        # Broadcast claim event (best-effort)
        await self.nostr_poster.post_event(
            KIND_CLAIM,
            tags=[
                ["job_id", job_id],
                ["worker", self.worker_id],
                ["escrow", task["escrow_contract"]],
            ],
        )

        self.processed_jobs[job_id] = True
        # Evict oldest entries when over limit (FIFO)
        while len(self.processed_jobs) > self.max_processed:
            self.processed_jobs.popitem(last=False)

        # 3. Execute the task
        result = await self._execute_task(task)
        if not result:
            log.error("Task execution failed for %s", job_id)
            return

        # 4. Submit result
        try:
            self._submit_result(task["escrow_contract"], job_id, result)
            log.info("Submitted result for %s", job_id)
        except Exception as e:
            log.error("Failed to submit result for %s: %s", job_id, e)
            return

        # Broadcast result event (best-effort, only after on-chain success)
        await self.nostr_poster.post_event(
            KIND_RESULT,
            tags=[
                ["job_id", job_id],
                ["worker", self.worker_id],
                ["escrow", task["escrow_contract"]],
            ],
            content=result[:1000],  # Truncate — Nostr events have size limits
        )

    async def _wait_for_open(self, contract: str, job_id: str) -> dict | None:
        """Poll escrow until it reaches Open status.

        In msig-v2, there's a race: the Nostr event may arrive before the
        relayer has created the escrow on-chain, and before the agent has
        funded it. We poll with retries.

        Returns escrow dict if Open, None if we give up.
        """
        deadline = time.monotonic() + MAX_WAIT_SECONDS
        attempts = 0

        while time.monotonic() < deadline:
            escrow = self._get_escrow(contract, job_id)
            if escrow:
                status = escrow.get("status")
                if status == "Open":
                    return escrow
                if status not in ("PendingFunding", None):
                    # Terminal or unexpected state — stop polling
                    log.debug("Escrow %s in state %s — not waiting", job_id, status)
                    return None

            attempts += 1
            elapsed = int(time.monotonic() - (deadline - MAX_WAIT_SECONDS))
            log.debug(
                "Escrow %s not ready yet (attempt %d, %ds elapsed) — retrying in %ds",
                job_id, attempts, elapsed, POLL_INTERVAL_SECONDS,
            )
            await asyncio.sleep(POLL_INTERVAL_SECONDS)

        return None

    def _get_escrow(self, contract: str, job_id: str) -> dict | None:
        """View escrow state."""
        try:
            result = self.account.provider.view_call(
                contract,
                "get_escrow",
                json.dumps({"job_id": job_id}).encode(),
            )
            if result.get("result"):
                data = bytes(result["result"]).decode()
                return json.loads(data) if data and data != "null" else None
        except Exception as e:
            log.warning("view_call failed: %s", e)
        return None

    def _claim(self, contract: str, job_id: str):
        """Claim escrow on-chain. Attaches 0.1 NEAR worker stake."""
        args = json.dumps({"job_id": job_id}).encode("utf-8")
        self.account.function_call(
            contract,
            "claim",
            args,
            gas=100_000_000_000_000,  # 100 Tgas
            amount=100_000_000_000_000_000_000_000,  # 0.1 NEAR worker stake
        )

    def _submit_result(self, contract: str, job_id: str, result: str):
        """Submit work result on-chain."""
        args = json.dumps({"job_id": job_id, "result": result}).encode("utf-8")
        self.account.function_call(
            contract,
            "submit_result",
            args,
            gas=300_000_000_000_000,  # 300 Tgas (needs extra for yield)
            amount=0,
        )

    async def _execute_task(self, task: dict) -> str | None:
        """Execute the task and return the result string.

        This is where you plug in your actual agent logic.
        For now, returns a placeholder that you replace with real execution.
        """
        # TODO: Plug in actual agent execution (LLM call, code generation, etc.)
        # This could call an LLM API, run code, build something, etc.

        log.info("Executing task: %s", task["task_description"][:80])
        await asyncio.sleep(1)  # Placeholder for actual work

        result = json.dumps({
            "completed": True,
            "output": "Task execution placeholder — replace with real agent logic",
            "artifacts": [],
        })

        return result


def main():
    parser = argparse.ArgumentParser(description="NEAR Escrow Worker Agent")
    parser.add_argument("--config", "-c", default="config.json")
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

    worker = WorkerAgent(config)

    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)

    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, worker._signal_handler, sig, None)

    try:
        loop.run_until_complete(worker.watch())
    finally:
        log.info("Worker shutdown complete")
        loop.close()


if __name__ == "__main__":
    main()
