# Worker Result Submission Spec (Kind 41002)

Workers are external agents. They don't install daemon binaries or share code with the inlayer crate. Their only contract with the system is a Nostr event.

## Overview

After a worker sees kind 41004 (FUNDED) on Nostr and completes its work, it posts a single kind 41002 event containing:

- The work output
- Two pre-signed msig actions (claim + submit_result)
- The signatures for those actions

The daemon picks up 41002, validates the signatures, and relays the actions on-chain via `worker_msig.execute()`.

## Event Format

```
kind: 41002
content: JSON { "job_id": string, "output": string }
tags: (see below)
```

## Required Tags

| Tag name | Value | Description |
|----------|-------|-------------|
| `job_id` | string | Matches the escrow job_id |
| `worker_msig` | NEAR account ID | Worker's multisig account (e.g. `worker1.msig.testnet`) |
| `claim_action` | JSON string | Serialized msig action for `claim()` |
| `claim_sig` | hex string (64 bytes) | ed25519 signature of claim_action JSON |
| `submit_action` | JSON string | Serialized msig action for `submit_result()` |
| `submit_sig` | hex string (64 bytes) | ed25519 signature of submit_action JSON |

## Action Structure

Each action is a JSON object matching the msig `execute` contract interface:

```json
{
  "nonce": <number>,
  "action": {
    "type": "call",
    "receiver_id": "<escrow_contract>",
    "method": "<claim|submit_result>",
    "args": { ... },
    "gas": 100000000000000,
    "deposit": "<yocto_amount>"
  }
}
```

### Claim Action

```json
{
  "nonce": N,
  "action": {
    "type": "call",
    "receiver_id": "<escrow_contract>",
    "method": "claim",
    "args": { "job_id": "<job_id>" },
    "gas": 100000000000000,
    "deposit": "100000000000000000000000"
  }
}
```

- `nonce`: `current_msig_nonce + 1` (query via RPC `view msig_contract get_nonce`)
- `deposit`: Worker's stake (0.1 NEAR = 10^23 yoctoNEAR default). This is real value at risk.

### Submit Result Action

```json
{
  "nonce": N+1,
  "action": {
    "type": "call",
    "receiver_id": "<escrow_contract>",
    "method": "submit_result",
    "args": {
      "job_id": "<job_id>",
      "result": "{\"kv_account\":\"kv.fastnear.near\",\"kv_predecessor\":\"<worker_msig>\",\"kv_key\":\"result/<job_id>\"}"
    },
    "gas": 100000000000000,
    "deposit": "0"
  }
}
```

- `nonce`: `claim_nonce + 1` (sequential — both actions execute back-to-back via msig)
- `result`: JSON string of the KV reference (not the actual output). The daemon writes the full output to KV before relaying this action.

## Signing

1. Serialize the action object to a compact JSON string (no whitespace changes after signing)
2. Sign the UTF-8 bytes of that JSON string with the worker's ed25519 private key
3. Hex-encode the 64-byte signature

The key used for signing must be an authorized key on the worker's msig contract.

## KV Reference

The `result` field in submit_result args is a JSON-encoded KV reference:

```json
{
  "kv_account": "kv.fastnear.near",
  "kv_predecessor": "<worker_msig_account>",
  "kv_key": "result/<job_id>"
}
```

- `kv_account`: The FastNear KV contract. Default: `kv.fastnear.near` (testnet: `kv.fastnear.testnet`)
- `kv_predecessor`: The worker's msig account (used as KV namespace)
- `kv_key`: Deterministic format `result/{job_id}` — the worker knows this at sign time

The daemon writes the actual output to KV before relaying submit_result. The verifier later fetches the full output via:

```
GET https://kv.main.fastnear.com/v0/latest/{kv_account}/{kv_predecessor}/{kv_key}
```

## Worker Flow (Step by Step)

```
1. Listen for kind 41004 (FUNDED) on Nostr relays
2. Parse job_id from tags, decide if you want this task
3. Do the actual work off-chain (git clone, compute, whatever)
4. Query RPC for worker msig nonce: view <worker_msig> get_nonce {}
5. Build claim_action JSON with nonce = current + 1
6. Sign claim_action with ed25519 worker key → claim_sig (hex)
7. Build submit_action JSON with nonce = current + 2
8. Sign submit_action with ed25519 worker key → submit_sig (hex)
9. Build kind 41002 event:
   content = { "job_id": "...", "output": "<actual work output>" }
   tags = [job_id, worker_msig, claim_action, claim_sig, submit_action, submit_sig]
10. Sign event with secp256k1 (Nostr identity key) and publish to relay
```

## Example Event

```
{
  "kind": 41002,
  "content": "{\"job_id\":\"escrow-42\",\"output\":\"The analysis shows...\"}",
  "tags": [
    ["job_id", "escrow-42"],
    ["worker_msig", "worker1.msig.testnet"],
    ["claim_action", "{\"nonce\":15,\"action\":{\"type\":\"call\",\"receiver_id\":\"escrow.kampouse.testnet\",\"method\":\"claim\",\"args\":{\"job_id\":\"escrow-42\"},\"gas\":100000000000000,\"deposit\":\"100000000000000000000000\"}}"],
    ["claim_sig", "a1b2c3d4...64_bytes_hex"],
    ["submit_action", "{\"nonce\":16,\"action\":{\"type\":\"call\",\"receiver_id\":\"escrow.kampouse.testnet\",\"method\":\"submit_result\",\"args\":{\"job_id\":\"escrow-42\",\"result\":\"{\\\"kv_account\\\":\\\"kv.fastnear.testnet\\\",\\\"kv_predecessor\\\":\\\"worker1.msig.testnet\\\",\\\"kv_key\\\":\\\"result/escrow-42\\\"}\"},\"gas\":100000000000000,\"deposit\":\"0\"}}"],
    ["submit_sig", "e5f6a7b8...64_bytes_hex"]
  ],
  "pubkey": "<worker secp256k1 pubkey (npub hex)>",
  "created_at": 1713078000,
  "id": "<event_id>",
  "sig": "<nostr event signature>"
}
```

## What the Daemon Does With Your Event

1. Validates claim_sig and submit_sig against worker_msig authorized keys
2. Calls `worker_msig.execute(claim_action, claim_sig)` — stakes worker's funds
3. Writes the output from event content to FastNear KV at `result/{job_id}`
4. Calls `worker_msig.execute(submit_action, submit_sig)` — escrow enters Verifying
5. Verifier scores the work, escrow settles

The worker never touches RPC or on-chain directly. All on-chain actions flow through the daemon relaying pre-signed msig actions.

## Failure Modes

| Scenario | What happens |
|----------|-------------|
| Invalid signature | Daemon rejects event, no on-chain action |
| Wrong nonce | msig.execute fails, daemon logs error, retries won't help — worker must re-sign with correct nonce |
| Worker stake too low | claim() fails on-chain, daemon logs error |
| Timeout before submission | Escrow stays Open, eventually times out, agent gets refund |
| Bad work output | Verifier scores low, escrow settles Failed, worker loses stake |
| Daemon crashes mid-relay | Claim may have succeeded. On restart, daemon checks escrow state and resumes from where it left off |
