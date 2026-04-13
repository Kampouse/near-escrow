"""Post a task to Nostr as kind 41000 event.

Quick helper to test the Nostr → escrow flow.

Usage:
    python post_task.py --job_id test-001 --reward 1000000 --token usdt.tether-token.near \
        --description "Build a REST API" --criteria "Must have tests" --key <hex-privkey>
"""

import argparse
import hashlib
import json
import sys
import time

import websockets

try:
    from secp256k1 import schnorr
except ImportError:
    # Use @noble/curves via subprocess or pure python fallback
    pass


KIND_TASK = 41000


def schnorr_sign(event_id: str, privkey_hex: str) -> str:
    """Sign event ID with schnorr (BIP-340). Requires nostr SDK or noble/curves."""
    try:
        from nostr_sdk import Keys
        keys = Keys.from_hex_privkey(privkey_hex)
        # Build and sign via SDK
        return keys  # Will use SDK flow below
    except ImportError:
        pass

    # Fallback: use python-secp256k1 or raise
    log.error("Install nostr-sdk or provide signing implementation")
    sys.exit(1)


async def post_task(args):
    """Build and post a kind 41000 task event."""
    try:
        from nostr_sdk import Client, EventBuilder, Keys, Kind, Tag
    except ImportError:
        print("pip install nostr-sdk")
        sys.exit(1)

    keys = Keys.from_hex_privkey(args.key)
    client = Client()

    for relay in args.relays:
        await client.add_relay(relay)
    await client.connect()

    # Build tags
    tags_list = [
        ["job_id", args.job_id],
        ["reward", args.reward, args.token],
        ["timeout", str(args.timeout)],
        ["agent", args.agent],
        ["escrow", args.escrow],
    ]
    if args.verifier_fee:
        tags_list.append(["verifier_fee", args.verifier_fee])
    if args.threshold:
        tags_list.append(["score_threshold", str(args.threshold)])
    if args.category:
        tags_list.append(["category", args.category])

    content = json.dumps({
        "task_description": args.description,
        "criteria": args.criteria,
    })

    # Build and sign event
    builder = EventBuilder(
        kind=Kind(KIND_TASK),
        content=content,
    )

    for tag in tags_list:
        builder = builder.tag(Tag.custom(tag))

    event = builder.to_event(keys)

    # Send
    event_id = await client.send_event(event)
    print(f"✅ Task posted: {event_id}")
    print(f"   job_id: {args.job_id}")
    print(f"   reward: {args.reward} {args.token}")
    print(f"   description: {args.description[:80]}...")

    # Keep connection alive briefly
    await asyncio.sleep(2)
    await client.disconnect()


if __name__ == "__main__":
    import asyncio
    import logging
    log = logging.getLogger(__name__)

    parser = argparse.ArgumentParser(description="Post task to Nostr")
    parser.add_argument("--key", required=True, help="Nostr private key (hex)")
    parser.add_argument("--job_id", required=True)
    parser.add_argument("--reward", required=True, help="Amount in smallest unit")
    parser.add_argument("--token", required=True, help="FT contract account ID")
    parser.add_argument("--timeout", type=int, default=24, help="Hours")
    parser.add_argument("--agent", required=True, help="NEAR account ID of agent")
    parser.add_argument("--escrow", required=True, help="Escrow contract account ID")
    parser.add_argument("--description", required=True)
    parser.add_argument("--criteria", required=True)
    parser.add_argument("--verifier_fee", default=None)
    parser.add_argument("--threshold", type=int, default=80)
    parser.add_argument("--category", default=None)
    parser.add_argument("--relays", nargs="+", default=[
        "wss://nostr-relay-production.up.railway.app/"
    ])

    args = parser.parse_args()
    asyncio.run(post_task(args))
