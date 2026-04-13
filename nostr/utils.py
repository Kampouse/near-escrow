"""Shared utilities for Nostr-based msig action signing."""

import base58
from nacl.signing import SigningKey


def get_nonce(rpc_url: str, msig_address: str) -> int:
    """Query the msig contract for current nonce via NEAR RPC."""
    from near_api.providers import JsonProvider

    provider = JsonProvider(rpc_url)
    result = provider.view_call(
        msig_address,
        "get_nonce",
        b"",
    )
    if result.get("result"):
        data = bytes(result["result"]).decode()
        return int(data)
    return 0


def sign_action_ed25519(action_json: str, private_key_str: str) -> bytes:
    """Sign action JSON with ed25519 private key.

    Args:
        action_json: Canonical JSON string of the action
        private_key_str: Key in "ed25519:base58..." format

    Returns:
        64-byte ed25519 signature
    """
    # Strip the ed25519: prefix
    stripped = private_key_str
    if stripped.startswith("ed25519:"):
        stripped = stripped[8:]

    raw = base58.b58decode(stripped)

    # NEAR keys can be 32 bytes (seed) or 64 bytes (seed + public)
    if len(raw) == 64:
        seed = raw[:32]
    elif len(raw) == 32:
        seed = raw
    else:
        raise ValueError(f"Invalid ed25519 key length: {len(raw)} bytes (expected 32 or 64)")

    sk = SigningKey(seed)
    signature = sk.sign(action_json.encode("utf-8")).signature
    return signature
