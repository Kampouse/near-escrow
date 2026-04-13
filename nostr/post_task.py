"""Post a task to Nostr with a signed msig CreateEscrow action.

The agent:
1. Queries msig.get_nonce() for current nonce
2. Builds a CreateEscrow action with nonce+1
3. Signs the action JSON with its ed25519 private key
4. Embeds the signed action in a Nostr event (kind 41000)
5. Posts to relays

The relayer extracts the signed action and calls msig.execute().

Two keys required:
  --nostr-key  : secp256k1 private key (hex) for Nostr event signing (identity)
  --agent-key  : ed25519 private key (ed25519:base58...) for msig action signing (authorization)

Usage:
    python post_task.py \
        --msig agent-abc.near \
        --escrow escrow.kampouse.near \
        --job_id test-001 --reward 1000000 --token usdt.tether-token.near \
        --description "Build a REST API" --criteria "Must have tests" \
        --nostr-key <hex> --agent-key ed25519:base58... \
        --rpc https://rpc.testnet.near.org
"""

import argparse
import asyncio
import json
import logging
import sys

from nostr.utils import get_nonce, sign_action_ed25519

KIND_TASK = 41000

log = logging.getLogger(__name__)


async def post_task(args):
    """Build and post a kind 41000 task event with signed msig action."""
    try:
        from nostr_sdk import Client, EventBuilder, Keys, Kind, Tag
    except ImportError:
        print("pip install nostr-sdk")
        sys.exit(1)

    # 1. Get current nonce from msig contract
    log.info("Querying nonce from msig %s", args.msig)
    current_nonce = get_nonce(args.rpc, args.msig)
    next_nonce = current_nonce + 1
    log.info("Current nonce: %d, next: %d", current_nonce, next_nonce)

    # 2. Build the CreateEscrow action — must match ActionKind in msig contract
    action = {
        "nonce": next_nonce,
        "action": {
            "type": "create_escrow",
            "job_id": args.job_id,
            "amount": args.reward,
            "token": args.token,
            "timeout_hours": args.timeout,
            "task_description": args.description,
            "criteria": args.criteria,
        },
    }
    if args.verifier_fee:
        action["action"]["verifier_fee"] = args.verifier_fee
    if args.threshold is not None:
        action["action"]["score_threshold"] = args.threshold

    # Deterministic JSON: sort_keys + compact separators
    action_json = json.dumps(action, separators=(",", ":"), sort_keys=True)
    log.info("Action JSON: %s", action_json)

    # 3. Sign the action with ed25519 key
    signature = sign_action_ed25519(action_json, args.agent_key)
    sig_hex = signature.hex()
    log.info("Signature: %s... (%d bytes)", sig_hex[:16], len(signature))

    # 4. Build Nostr event
    keys = Keys.from_hex_privkey(args.nostr_key)
    client = Client()

    for relay in args.relays:
        await client.add_relay(relay)
    await client.connect()

    # Tags: discovery metadata + signed action payload
    tags_list = [
        ["job_id", args.job_id],
        ["reward", args.reward, args.token],
        ["timeout", str(args.timeout)],
        ["agent", args.msig],               # msig contract address
        ["escrow", args.escrow],
        ["action", action_json],             # signed action payload
        ["action_sig", sig_hex],             # ed25519 signature (hex)
    ]

    if args.npub:
        tags_list.append(["npub", args.npub])
    if args.verifier_fee:
        tags_list.append(["verifier_fee", args.verifier_fee])
    if args.threshold is not None:
        tags_list.append(["score_threshold", str(args.threshold)])
    if args.category:
        tags_list.append(["category", args.category])
    if args.skills:
        for skill in args.skills:
            tags_list.append(["skills", skill])

    content = json.dumps({
        "task_description": args.description,
        "criteria": args.criteria,
    })

    builder = EventBuilder(
        kind=Kind(KIND_TASK),
        content=content,
    )
    for tag in tags_list:
        builder = builder.tag(Tag.custom(tag))

    event = builder.to_event(keys)
    event_id = await client.send_event(event)

    print(f"Task posted: {event_id}")
    print(f"  msig:      {args.msig}")
    print(f"  job_id:    {args.job_id}")
    print(f"  nonce:     {next_nonce}")
    print(f"  reward:    {args.reward} {args.token}")
    print(f"  desc:      {args.description[:80]}...")

    await asyncio.sleep(2)
    await client.disconnect()


if __name__ == "__main__":
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s: %(message)s",
    )

    parser = argparse.ArgumentParser(
        description="Post task to Nostr with signed msig CreateEscrow action"
    )

    # Nostr identity
    parser.add_argument(
        "--nostr-key", required=True,
        help="Nostr secp256k1 private key (hex) for event signing",
    )
    parser.add_argument(
        "--npub", default=None,
        help="Agent's Nostr public key (hex) — added as tag for worker verification",
    )

    # msig and escrow
    parser.add_argument(
        "--msig", required=True,
        help="Agent msig contract address (NEAR account ID)",
    )
    parser.add_argument(
        "--escrow", required=True,
        help="Escrow contract address (NEAR account ID)",
    )
    parser.add_argument(
        "--rpc", default="https://rpc.testnet.near.org",
        help="NEAR RPC URL",
    )

    # Task details
    parser.add_argument("--job_id", required=True)
    parser.add_argument("--reward", required=True, help="Amount in smallest unit")
    parser.add_argument("--token", required=True, help="FT contract account ID")
    parser.add_argument("--timeout", type=int, default=24, help="Hours")
    parser.add_argument("--description", required=True)
    parser.add_argument("--criteria", required=True)
    parser.add_argument("--verifier_fee", default=None)
    parser.add_argument("--threshold", type=int, default=None)
    parser.add_argument("--category", default=None)
    parser.add_argument("--skills", nargs="*", default=[])

    # Agent ed25519 key for msig action signing
    parser.add_argument(
        "--agent-key", required=True,
        help="Ed25519 private key for signing msig actions (ed25519:base58...)",
    )

    parser.add_argument("--relays", nargs="+", default=[
        "wss://nostr-relay-production.up.railway.app/"
    ])

    args = parser.parse_args()
    asyncio.run(post_task(args))
