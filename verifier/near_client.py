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

    def get_verifying_escrows(self) -> list[dict]:
        """Fetch escrows in Verifying state directly from contract view.

        Uses the list_verifying() view method which returns job_id + data_id + result.
        No block scanning needed.
        """
        try:
            result = self.provider.view_call(
                self.contract_id,
                "list_verifying",
                b"",
            )
            if result.get("result"):
                data = bytes(result["result"]).decode()
                if data and data != "null":
                    return json.loads(data)
        except Exception as e:
            log.warning("view_call list_verifying failed: %s", e)
        return []

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
        args = {
            "data_id_hex": data_id_hex,
            "verdict": verdict,
        }

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
