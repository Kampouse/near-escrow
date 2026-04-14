"""DEPRECATED — Use the Rust CLI instead.

This Python script is superseded by `inlayer post-task`.
The Rust version handles CreateEscrow + FundEscrow signing in one command.

    Rust equivalent: near-inlayer/worker/src/daemon/escrow_commands.rs
    CLI: inlayer post-task --nostr-key ... --agent-key ... --msig ...

Kept for reference only.

---

Sign and post a generic msig action to Nostr (kind 41003).

Used for FundEscrow, CancelEscrow, RegisterToken, RotateKey, Withdraw.
The agent signs the action JSON with its ed25519 key and posts it as a
Nostr event. The relayer picks it up and calls msig.execute().

Usage:
    # Fund an escrow
    python sign_action.py \
        --msig agent-abc.near \
        --action fund_escrow \
        --params '{"job_id":"test-001","token":"usdt.tether-token.near","amount":"1000000"}' \
        --agent-key ed25519:base58... --nostr-key <hex>

    # Cancel an escrow
    python sign_action.py \
        --msig agent-abc.near \
        --action cancel_escrow \
        --params '{"job_id":"test-001"}' \
        --agent-key ed25519:base58... --nostr-key <hex>

    # Withdraw FT tokens
    python sign_action.py \
        --msig agent-abc.near \
        --action withdraw \
        --params '{"token":"usdt.tether-token.near","amount":"500000","recipient":"bob.near"}' \
        --agent-key ed25519:base58... --nostr-key <hex>

    # Withdraw NEAR (token=null)
    python sign_action.py \
        --msig agent-abc.near \
        --action withdraw \
        --params '{"amount":"1000000000000000000000000","recipient":"bob.near"}' \
        --agent-key ed25519:base58... --nostr-key <hex>

    # Rotate key
    python sign_action.py \
        --msig agent-abc.near \
        --action rotate_key \
        --params '{"new_pubkey":"ed25519:..."}' \
        --agent-key ed25519:base58... --nostr-key <hex>
"""

import argparse
import asyncio
import json
import logging
import sys

from nostr.utils import get_nonce, sign_action_ed25519

KIND_ACTION = 41003

log = logging.getLogger(__name__)


# Map CLI action names to msig ActionKind type tags
ACTION_TYPE_MAP = {
    "create_escrow": "create_escrow",
    "fund_escrow": "fund_escrow",
    "cancel_escrow": "cancel_escrow",
    "register_token": "register_token",
    "rotate_key": "rotate_key",
    "withdraw": "withdraw",
}


async def sign_and_post(args):
    """Sign the action and post to Nostr as kind 41003."""
    try:
        from nostr_sdk import Client, EventBuilder, Keys, Kind, Tag
    except ImportError:
        print("pip install nostr-sdk")
        sys.exit(1)

    # Validate action type
    action_type = ACTION_TYPE_MAP.get(args.action)
    if not action_type:
        print(f"Unknown action: {args.action}")
        print(f"Valid actions: {', '.join(ACTION_TYPE_MAP.keys())}")
        sys.exit(1)

    # Parse params
    try:
        params = json.loads(args.params)
    except json.JSONDecodeError as e:
        print(f"Invalid JSON in --params: {e}")
        sys.exit(1)

    # Get current nonce
    log.info("Querying nonce from msig %s", args.msig)
    current_nonce = get_nonce(args.rpc, args.msig)
    next_nonce = current_nonce + 1
    log.info("Current nonce: %d, next: %d", current_nonce, next_nonce)

    # Build action — must match ActionKind enum in msig contract
    action = {
        "nonce": next_nonce,
        "action": {
            "type": action_type,
            **params,
        },
    }

    # Deterministic JSON
    action_json = json.dumps(action, separators=(",", ":"), sort_keys=True)
    log.info("Action JSON: %s", action_json)

    # Sign
    signature = sign_action_ed25519(action_json, args.agent_key)
    sig_hex = signature.hex()
    log.info("Signature: %s... (%d bytes)", sig_hex[:16], len(signature))

    # Build Nostr event
    keys = Keys.from_hex_privkey(args.nostr_key)
    client = Client()

    for relay in args.relays:
        await client.add_relay(relay)
    await client.connect()

    tags_list = [
        ["agent", args.msig],
        ["action", action_json],
        ["action_sig", sig_hex],
    ]

    builder = EventBuilder(kind=Kind(KIND_ACTION), content="")
    for tag in tags_list:
        builder = builder.tag(Tag.custom(tag))

    event = builder.to_event(keys)
    event_id = await client.send_event(event)

    print(f"Action posted: {event_id}")
    print(f"  msig:   {args.msig}")
    print(f"  type:   {action_type}")
    print(f"  nonce:  {next_nonce}")

    await asyncio.sleep(2)
    await client.disconnect()


if __name__ == "__main__":
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s: %(message)s",
    )

    parser = argparse.ArgumentParser(
        description="Sign and post a generic msig action to Nostr"
    )

    parser.add_argument("--msig", required=True, help="Agent msig contract address")
    parser.add_argument(
        "--action", required=True,
        choices=list(ACTION_TYPE_MAP.keys()),
        help="Action type to execute",
    )
    parser.add_argument(
        "--params", required=True,
        help="Action parameters as JSON string",
    )
    parser.add_argument("--agent-key", required=True, help="Ed25519 private key (ed25519:base58...)")
    parser.add_argument("--nostr-key", required=True, help="Nostr private key (hex)")
    parser.add_argument("--rpc", default="https://rpc.testnet.near.org", help="NEAR RPC URL")
    parser.add_argument("--relays", nargs="+", default=[
        "wss://nostr-relay-production.up.railway.app/"
    ])

    args = parser.parse_args()
    asyncio.run(sign_and_post(args))
