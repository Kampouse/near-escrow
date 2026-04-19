"""NEAR RPC client for the verifier service.

Handles:
- Polling for `result_submitted` events (escrow entering Verifying state)
- Reading escrow state via view calls
- Calling resume_verification to deliver the verdict via promise_yield_resume
"""

import json
import logging
from typing import Optional
from urllib.request import urlopen, Request
from urllib.error import URLError

from near_api.account import Account
from near_api.providers import JsonProvider

log = logging.getLogger(__name__)

# FastNear KV endpoints
KV_URLS = {
    "testnet": "https://kv.testnet.fastnear.com/v0/latest/{account}/{predecessor}/{key}",
    "mainnet": "https://kv.main.fastnear.com/v0/latest/{account}/{predecessor}/{key}",
}


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

    def get_verifying_escrows(self, from_index: int = 0, limit: int = 100) -> list[dict]:
        """Fetch escrows in Verifying state directly from contract view.

        Uses the paginated list_verifying(from_index, limit) view method which
        returns job_id + data_id + status. Automatically paginates to fetch all
        verifying escrows. No block scanning needed.
        """
        all_escrows: list[dict] = []
        page_size = min(limit, 100)  # contract caps at 100
        offset = from_index

        while True:
            try:
                args = json.dumps({"from_index": offset, "limit": page_size}).encode()
                result = self.provider.view_call(
                    self.contract_id,
                    "list_verifying",
                    args,
                )
                if result.get("result"):
                    data = bytes(result["result"]).decode()
                    if data and data != "null":
                        page = json.loads(data)
                        if not page:
                            break
                        all_escrows.extend(page)
                        if len(page) < page_size:
                            break  # last page
                        offset += len(page)
                    else:
                        break
                else:
                    break
            except Exception as e:
                log.warning("view_call list_verifying failed at offset %d: %s", offset, e)
                break

        return all_escrows

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
    # FastNear KV — fetch work results
    # ------------------------------------------------------------------

    def fetch_kv_result(self, kv_reference: str, network: str = "testnet") -> Optional[str]:
        """Fetch work result from FastNear KV storage.

        Args:
            kv_reference: JSON string like {"kv_account":"...", "kv_predecessor":"...", "kv_key":"..."}
                          or a plain URL string.
            network: "testnet" or "mainnet" (used for URL template if kv_reference is JSON)

        Returns:
            The result text, or None on failure.
        """
        # Try parsing as JSON reference
        try:
            ref = json.loads(kv_reference) if isinstance(kv_reference, str) else kv_reference
            kv_account = ref.get("kv_account", "")
            kv_predecessor = ref.get("kv_predecessor", "")
            kv_key = ref.get("kv_key", "")

            if not all([kv_account, kv_predecessor, kv_key]):
                log.warning("Incomplete kv_reference: %s", kv_reference)
                return None

            url_template = KV_URLS.get(network, KV_URLS["testnet"])
            url = url_template.format(
                account=kv_account,
                predecessor=kv_predecessor,
                key=kv_key,
            )
        except (json.JSONDecodeError, AttributeError):
            # Maybe it's a plain URL
            if kv_reference.startswith("http"):
                url = kv_reference
            else:
                log.warning("Cannot parse kv_reference: %s", kv_reference)
                return None

        try:
            req = Request(url, headers={"Accept": "application/json"})
            with urlopen(req, timeout=10) as resp:
                data = json.loads(resp.read().decode())
                # FastNear KV returns the value directly
                # It might be nested under "value" or just the raw data
                if isinstance(data, dict):
                    # Could be {"value": "..."} or the actual result
                    result = data.get("value", data.get("result"))
                    if result is not None:
                        if isinstance(result, str):
                            return result
                        return json.dumps(result)
                    return json.dumps(data)
                return str(data)
        except URLError as e:
            log.error("KV fetch failed for %s: %s", url, e)
            return None
        except Exception as e:
            log.error("KV parse error for %s: %s", url, e)
            return None

    def resolve_escrow_result(self, escrow: dict, network: str = "testnet") -> Optional[str]:
        """Resolve the actual work result from an escrow, handling KV references.

        If the result is a KV reference (JSON with kv_account/kv_key), fetches from KV.
        Otherwise returns the result directly.
        """
        result = escrow.get("result")
        if not result:
            return None

        # Check if it's a KV reference
        try:
            ref = json.loads(result)
            if isinstance(ref, dict) and "kv_account" in ref:
                log.info("Fetching result from KV: %s/%s/%s", ref.get("kv_account"), ref.get("kv_predecessor"), ref.get("kv_key"))
                return self.fetch_kv_result(result, network)
        except (json.JSONDecodeError, AttributeError):
            pass

        # Plain text result — return as-is
        return result

    # ------------------------------------------------------------------
    # Helpers
    # ------------------------------------------------------------------

    def get_stats(self) -> dict:
        """Get contract stats (total escrows by status)."""
        result = self.provider.view_call(
            self.contract_id,
            "get_stats",
            b"",
        )
        if result.get("result"):
            return json.loads(bytes(result["result"]).decode())
        return {}
