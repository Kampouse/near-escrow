"""Worker Agent for NEAR Escrow Marketplace.

Subscribes to Nostr task events (kind 41000), evaluates if the task
matches capabilities, claims on-chain, executes the task, submits result.

Usage:
    export NEAR_WORKER_KEY=ed25519:...
    python worker.py --config config.json
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
    print("pip install near-api-py websockets")
    sys.exit(1)

log = logging.getLogger("worker")

KIND_TASK = 41000


def load_config(path: str) -> dict:
    with open(path) as f:
        return json.load(f)


class WorkerAgent:
    """Nostr-based worker agent that claims and executes tasks."""

    def __init__(self, config: dict):
        self.config = config
        self.relays = config.get("relays", [
            "wss://nostr-relay-production.up.railway.app/"
        ])
        self.escrow_contract = config.get("escrow_contract")
        self.capabilities = config.get("capabilities", [])
        self.max_reward = config.get("max_reward", "10000000")
        self.processed_jobs: set[str] = set()
        self.max_processed = config.get("max_processed_cache", 10000)

        # NEAR client
        rpc_url = config.get("rpc_url", "https://rpc.testnet.near.org")
        worker_id = config.get("worker_account_id")

        key_str = os.environ.get("NEAR_WORKER_KEY", "")
        if not key_str or not worker_id:
            log.error("NEAR_WORKER_KEY and worker_account_id required")
            sys.exit(1)

        provider = JsonProvider(rpc_url)
        signer = Signer(worker_id, key_str)
        self.account = Account(provider, signer)
        self.worker_id = worker_id

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

        tags = {t[0]: t[1:] for t in event.get("tags", []) if len(t) >= 2}
        content = {}
        try:
            content = json.loads(event.get("content", "{}"))
        except json.JSONDecodeError:
            return

        reward_parts = tags.get("reward", ["0"])
        job_id = tags.get("job_id", [None])[0]
        if not job_id or job_id in self.processed_jobs:
            return

        task = {
            "job_id": job_id,
            "reward_amount": reward_parts[0] if reward_parts else "0",
            "timeout_hours": int(tags.get("timeout", ["24"])[0]),
            "escrow_contract": tags.get("escrow", [self.escrow_contract])[0],
            "category": tags.get("category", [None])[0],
            "skills": tags.get("skills", []),
            "task_description": content.get("task_description", ""),
            "criteria": content.get("criteria", ""),
        }

        # Check if we can do this task
        if not self.matches_capabilities(task):
            log.debug("Skipping %s — doesn't match capabilities", job_id)
            return

        log.info("🎯 Task matched: %s — %s", job_id, task["task_description"][:80])

        # 1. Check escrow is funded (Open status)
        escrow = self._get_escrow(task["escrow_contract"], job_id)
        if not escrow or escrow.get("status") != "Open":
            log.warning("Escrow %s not open — skipping", job_id)
            return

        # 2. Claim
        try:
            self._claim(task["escrow_contract"], job_id)
            log.info("✅ Claimed job %s", job_id)
        except Exception as e:
            log.error("Failed to claim %s: %s", job_id, e)
            return

        self.processed_jobs.add(job_id)
        if len(self.processed_jobs) > self.max_processed:
            self.processed_jobs = set(list(self.processed_jobs)[-self.max_processed:])

        # 3. Execute the task
        result = await self._execute_task(task)
        if not result:
            log.error("Task execution failed for %s", job_id)
            return

        # 4. Submit result
        try:
            self._submit_result(task["escrow_contract"], job_id, result)
            log.info("✅ Submitted result for %s", job_id)
        except Exception as e:
            log.error("Failed to submit result for %s: %s", job_id, e)

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
        """Claim escrow on-chain."""
        args = json.dumps({"job_id": job_id}).encode("utf-8")
        self.account.function_call(
            contract,
            "claim",
            args,
            gas=100_000_000_000_000,  # 100 Tgas
            amount=0,
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
    asyncio.run(worker.watch())


if __name__ == "__main__":
    main()
