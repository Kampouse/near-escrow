"""NEAR RPC client for the verifier service.

Handles:
- Polling for `result_submitted` events (escrow entering Verifying state)
- Reading escrow state via view calls
- Calling resume_verification to deliver the verdict via promise_yield_resume
"""

import json
import logging
from typing import Optional

from near_api.account import Account
from near_api.providers import JsonProvider

log = logging.getLogger(__name__)


class NearClient:
    """Wraps near-api-py for the operations the verifier needs."""

    def __init__(self, config: dict, account: Account):
        self.config = config
        self.account = account
        self.provider: JsonProvider = account.provider
        self.contract_id = config["contract_id"]

    # ------------------------------------------------------------------
    # View calls
    # ------------------------------------------------------------------

    def get_escrow(self, job_id: str) -> Optional[dict]:
        """Fetch escrow details by job_id. Returns None if not found."""
        args = json.dumps({"job_id": job_id}).encode()
        try:
            result = self.provider.view_call(
                self.contract_id,
                "get_escrow",
                args,
            )
            if "result" not in result or result["result"] is None:
                return None
            # RPC returns result as list of byte values
            data = bytes(result["result"]).decode()
            if not data or data == "null":
                return None
            return json.loads(data)
        except Exception as e:
            log.warning("view_call get_escrow failed for %s: %s", job_id, e)
            return None

    # ------------------------------------------------------------------
    # Event polling — scan recent blocks for result_submitted events
    # ------------------------------------------------------------------

    def get_recent_result_submitted_events(
        self, after_timestamp_ns: int
    ) -> list[dict]:
        """Fetch recent blocks and scan logs for `result_submitted` events.

        Events follow the NEAR Events standard:
        {
          "standard": "escrow",
          "version": "3.0.0",
          "event": "result_submitted",
          "data": [{"job_id": "...", "data_id": "..."}]
        }

        For production, replace this with a FastNear indexer subscription.
        """
        events = []

        status = self.provider.get_status()
        latest_height = int(status["sync_info"]["latest_block_height"])

        # Scan last ~200 blocks (~2 min — matches yield timeout window)
        start = max(0, latest_height - 200)

        for height in range(start, latest_height + 1):
            try:
                block = self.provider.get_block(height)
                block_ts = int(block["header"]["timestamp_nanosec"])
                if block_ts <= after_timestamp_ns:
                    continue

                for chunk_info in block.get("chunks", []):
                    if not chunk_info.get("tx_root") or chunk_info["tx_root"] == "11111111111111111111111111111111":
                        continue
                    try:
                        chunk = self.provider.get_chunk(chunk_info["chunk_hash"])
                    except Exception:
                        continue

                    for tx in chunk.get("transactions", []):
                        if tx.get("receiver_id") != self.contract_id:
                            continue
                        try:
                            receipt = self.provider.get_tx(tx["hash"], tx["signer_id"])
                            for outcome in receipt.get("receipts_outcome", []):
                                for log_line in outcome.get("outcome", {}).get("logs", []):
                                    event = self._parse_event(log_line)
                                    if event and event.get("event") == "result_submitted":
                                        for d in event.get("data", []):
                                            d["_block_height"] = height
                                            d["_timestamp_ns"] = block_ts
                                            events.append(d)
                        except Exception:
                            continue
            except Exception:
                continue

        return events

    @staticmethod
    def _parse_event(log_line: str) -> Optional[dict]:
        """Parse a NEAR Events standard log line."""
        if not log_line.startswith("EVENT_JSON:"):
            return None
        try:
            return json.loads(log_line[len("EVENT_JSON:"):])
        except (json.JSONDecodeError, ValueError):
            return None

    # ------------------------------------------------------------------
    # Resume — deliver verdict to contract
    # ------------------------------------------------------------------

    def resume_yield(self, data_id_hex: str, verdict: str) -> str:
        """Call resume_verification on the contract.

        The contract's resume_verification method decodes the hex data_id,
        builds the payload bytes, and calls env::promise_yield_resume()
        internally. This triggers the verification_callback.

        Args:
            data_id_hex: hex-encoded 32-byte CryptoHash from the event
            verdict: JSON string {"score": 85, "passed": true, "detail": "..."}

        Returns:
            Transaction hash
        """
        args = json.dumps({
            "data_id_hex": data_id_hex,
            "verdict": verdict,
        }).encode("utf-8")

        result = self.account.function_call(
            self.contract_id,
            "resume_verification",
            args,
            gas=300_000_000_000_000,  # 300 Tgas
            amount=0,
        )

        return result

    # ------------------------------------------------------------------
    # Helpers
    # ------------------------------------------------------------------

    def get_stats(self) -> dict:
        """Get contract stats (total escrows by status)."""
        result = self.provider.view_call(
            self.contract_id,
            "get_stats",
            "",
        )
        if result.get("result"):
            return json.loads(bytes(result["result"]).decode())
        return {}
