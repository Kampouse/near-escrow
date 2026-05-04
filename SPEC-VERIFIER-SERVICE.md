# Near-Escrow Verifier Service

## Overview

A self-hosted service that combines a Radicle P2P node, Nostr relay client, containerized task execution, and NEAR blockchain signing into a single deployment. Verifiers host task repos, run verification, and earn fees for doing so.

This is the missing piece that makes near-escrow usable end-to-end.

## Three-Layer Architecture

```
Nostr     = discovery, coordination, signed action relay, lifecycle events
Radicle   = git hosting, task delivery, access control, audit trail
NEAR      = payment, escrow, settlement
```

Each layer does one thing. No REST API needed — Nostr is the coordination layer.

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                    Verifier Service                       │
│                                                           │
│  ┌──────────────┐  ┌──────────────┐  ┌───────────────┐  │
│  │  Radicle Node │  │   Executor   │  │  NEAR Signer  │  │
│  │              │  │              │  │               │  │
│  │  • Git host  │  │  • Container │  │  • ed25519    │  │
│  │  • P2P sync  │  │    runtime   │  │  • Submit     │  │
│  │  • Access    │──│  • Sandbox   │──│    verdict    │  │
│  │    control   │  │  • verify.sh │  │  • Watch      │  │
│  │  • Private   │  │              │  │    escrows    │  │
│  │    repos     │  │              │  │               │  │
│  └──────┬───────┘  └──────────────┘  └───────┬───────┘  │
│         │                                      │          │
│  ┌──────┴──────────────────────────────────────┴───────┐  │
│  │              Nostr Client (nostr-sdk)                │  │
│  │  • Watch kind 41004 (FUNDED) → start verification   │  │
│  │  • Post kind 41006 (VERIFIED) → announce verdict     │  │
│  │  • Watch kind 41002 (WORKER_RESULT) → trigger verify │  │
│  └─────────────────────────────────────────────────────┘  │
│                                                           │
│  ┌─────────────────────────────────────────────────────┐  │
│  │              SQLite (local state)                    │  │
│  │  escrow_id ↔ repo_rid, verdicts, logs, reputation  │  │
│  └─────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────┘
         │              │              │
    ┌────┴───┐    ┌────┴───┐    ┌────┴────┐
    │ Agents  │    │Workers │    │  NEAR   │
    │(create  │    │(clone, │    │Contract │
    │ repos,  │    │ work,  │    │(settle) │
    │ post    │    │ push)  │    │         │
    └────────┘    └────────┘    └─────────┘
         │              │
    ┌────┴──────────────┴───┐
    │   Nostr Relays        │
    │  (wss://relay...)     │
    └───────────────────────┘
```

## Stack

| Component | Technology | Why |
|-----------|-----------|-----|
| Coordination | Nostr (nostr-sdk Rust) | Discovery, signed action relay, lifecycle events |
| Git hosting | Radicle Node | P2P, private repos, self-certifying, no central server |
| Task transport | Git (via Radicle) | Repo structure = task format, diffs = audit trail |
| Execution | Docker/Podman | Sandboxed, reproducible, resource-limited |
| Signing | ed25519 | Same key pair the NEAR contract expects |
| Database | SQLite | Local state: escrow↔repo mapping, verdicts, reputation |
| Config | TOML | Single config file |

## Nostr Event Kinds (extended)

| Kind | Name | Direction | Description |
|------|------|-----------|-------------|
| 41000 | TASK | agent → relayer → chain | Task announcement with signed action + repo_rid |
| 41002 | WORKER_RESULT | worker → relayer → chain | Worker submits result with signed action + commit_sha |
| 41003 | ACTION | agent → relayer → chain | Generic signed action (fund, cancel, withdraw) |
| 41004 | FUNDED | daemon → workers, verifiers | Escrow created + funded on-chain |
| 41005 | CONFIRMED | daemon → all | Settlement complete |
| 41006 | VERIFIED | verifier → all | Verification result (pass/fail, score, detail) |

See `nostr/event_schema.json` for full tag specifications.

## Repository Structure (Task Format)

Every task is a Radicle repository. The repo IS the task spec, input data, verification logic, and output — all in one place.

```
task-repo/
├── MANIFEST.json          # Task metadata (runtime, verification method, thresholds)
├── input/                 # Agent provides: data, code, prompts
│   ├── dataset.csv
│   ├── source.py
│   └── requirements.txt
├── verify/                # Agent provides: how to judge the work
│   ├── verify.sh          # Entry point (always)
│   ├── test_task.py       # Test suite
│   └── expected/          # Expected outputs for deterministic checks
│       └── output.txt
└── output/                # Worker creates: their work product
    ├── model.pkl
    ├── result.json
    └── solution.py
```

### MANIFEST.json

```json
{
  "version": 1,
  "name": "Train sentiment classifier",
  "task_type": "code",
  "created_by": "jemartel.near",
  "escrow_id": "42",
  "execution": {
    "method": "container",
    "runtime": "python:3.12-slim",
    "entrypoint": "python input/train.py --input /input --output /output",
    "timeout_seconds": 600,
    "memory_mb": 2048,
    "cpu_cores": 2,
    "network": false
  },
  "verification": {
    "method": "test_suite",
    "entrypoint": "/verify/verify.sh",
    "timeout_seconds": 120,
    "criteria": "All tests pass. F1 > 0.85."
  }
}
```

### verify.sh (always the entry point for verification)

```bash
#!/bin/bash
set -euo pipefail

# Exit 0 = pass, non-zero = fail
# Print verdict info to stdout (captured by executor)

cd /task
pytest verify/ -v --tb=short
```

Exit code 0 = passed. Non-zero = failed. Stdout is captured as verdict detail.

## Full Lifecycle

### 1. Agent Creates Task

```
Agent               Nostr Relay          Relayer Daemon        NEAR
  │                    │                      │                 │
  │ rad init --private │                      │                 │
  │ (MANIFEST.json,    │                      │                 │
  │  input/, verify/)  │                      │                 │
  │                    │                      │                 │
  │ rad id update      │                      │                 │
  │  --allow <verifier_DID>                  │                 │
  │  --allow <worker_DID>                    │                 │
  │                    │                      │                 │
  │ rad push           │                      │                 │
  │ (to verifier seed) │                      │                 │
  │                    │                      │                 │
  │ Nostr kind 41000   │                      │                 │
  │ (signed action +   │                      │                 │
  │  repo_rid tag)     │                      │                 │
  │───────────────────>│                      │                 │
  │                    │ kind 41000           │                 │
  │                    │─────────────────────>│                 │
  │                    │                      │ msig.execute()  │
  │                    │                      │────────────────>│
  │                    │                      │ escrow created  │
  │                    │                      │<────────────────│
  │                    │ kind 41004 (FUNDED)  │                 │
  │                    │<─────────────────────│                 │
  │                    │ kind 41004           │                 │
  │<───────────────────│                      │                 │
```

### 2. Worker Claims & Executes

```
Worker              Nostr Relay          Verifier           NEAR
  │                    │                    │                 │
  │ See kind 41004     │                    │                 │
  │<───────────────────│                    │                 │
  │                    │                    │                 │
  │ rad clone <RID>    │                    │                 │
  │ (from verifier)    │                    │                 │
  │<────────────────────────────────────────│                 │
  │                    │                    │                 │
  │ (read MANIFEST.json)                    │                 │
  │ (execute per execution spec)            │                 │
  │ (produce output/)                       │                 │
  │                    │                    │                 │
  │ rad push           │                    │                 │
  │ (output/ branch)   │                    │                 │
  │────────────────────────────────────────>│                 │
  │                    │                    │                 │
  │ Nostr kind 41002   │                    │                 │
  │ (signed claim +    │                    │                 │
  │  submit + commit_sha)                   │                 │
  │───────────────────>│                    │                 │
  │                    │ kind 41002         │                 │
  │                    │─────────────────────────────────────>│
  │                    │                    │ claim + submit  │
  │                    │                    │                 │
```

### 3. Verifier Checks & Signs

```
Verifier            Nostr Relay          NEAR
  │                    │                    │
  │ See kind 41002     │                    │
  │<───────────────────│                    │
  │                    │                    │
  │ rad checkout       │                    │
  │  <commit_sha>      │                    │
  │                    │                    │
  │ docker run         │                    │
  │  verify.sh         │                    │
  │  (sandboxed)       │                    │
  │                    │                    │
  │ Sign verdict       │                    │
  │ (ed25519)          │                    │
  │                    │                    │
  │ submit_verdict()   │                    │
  │────────────────────────────────────────>│
  │                    │                    │ settlement
  │                    │                    │<────────────────
  │                    │                    │
  │ Nostr kind 41006   │                    │
  │ (VERIFIED)         │                    │
  │───────────────────>│                    │
  │                    │ kind 41006         │
  │                    │ → all participants │
```

## Verification Methods

### Deterministic (exit code + hash comparison)
```bash
# verify.sh
diff <(sha256sum output/*) <(sha256sum verify/expected/*)
```
Use for: math, data transforms, pure functions.
Trust: cryptographic.

### Test Suite (exit code)
```bash
# verify.sh
pytest verify/ -v
```
Use for: bug fixes, features, code quality.
Trust: high (agent defines tests, verifier runs them).

### LLM Judge (score output)
```bash
# verify.sh
SCORE=$(curl -s verifier-internal:8080/judge \
  --json "{\"criteria\": \"$CRITERIA\", \"output\": \"$(cat output/result.json)\"}" \
  | jq -r '.score')
if (( $(echo "$SCORE >= 0.8" | bc -l) )); then
  echo "PASS (score: $SCORE)"
  exit 0
else
  echo "FAIL (score: $SCORE)"
  exit 1
fi
```
Use for: writing, translation, research, creative work.
Trust: moderate (judge model is configurable per task).

### Human Review (manual sign-off)
```bash
# verify.sh
# No automated verification
# Verifier posts kind 41006 with passed=false, score=0
# Human reviewers approve via separate Nostr DM or web UI
# Verifier re-posts kind 41006 with updated verdict
```
Use for: subjective work, high-value tasks.
Trust: social.

## Verifier Service Components

### rad-node (Git hosting + P2P)

Runs a Radicle node that:
- Hosts private task repos (agent creates, verifier seeds)
- Syncs with workers via Radicle P2P protocol
- Enforces access control (allow-lists via DIDs)
- Provides git push/pull over Radicle protocol
- Acts as a seed node for always-on availability

The verifier service starts and manages the Radicle node as a subprocess. No separate deployment.

### nostr-client (Coordination)

Connects to Nostr relays and:
- Subscribes to kind 41000 (TASK) — to discover tasks assigned to this verifier
- Subscribes to kind 41002 (WORKER_RESULT) — to trigger verification when worker submits
- Subscribes to kind 41004 (FUNDED) — to confirm escrow is funded before seeding repo
- Publishes kind 41006 (VERIFIED) — to announce verdict to all participants
- Watches kind 41005 (CONFIRMED) — to confirm settlement completed

No REST API. All coordination through Nostr events.

### executor (Container runtime)

Spawns Docker/Podman containers to run verification:
- Mounts task repo (read-only) + worker output (read-only) into container
- Enforces resource limits (memory, CPU, timeout, no network)
- Captures stdout/stderr/exit code
- Returns structured result: {passed, exit_code, output, duration}

### signer (NEAR integration)

Manages ed25519 key pair registered with the NEAR escrow contract:
- Watches for escrows where worker has submitted result
- Signs verdicts after verification
- Submits to contract via NEAR RPC
- Handles retries on settlement failure

### db (SQLite)

Local state not on-chain:
- escrow_id ↔ repo_rid mapping (from Nostr events)
- Verification results (pass/fail, logs, duration, commit_sha)
- Verifier reputation (accuracy, response time)
- Repo cleanup scheduling (TTL after settlement)

## Configuration

```toml
# verifier.toml

[service]
name = "verifier-1"

[near]
network = "testnet"
rpc_url = "https://rpc.testnet.near.org"
contract_id = "escrow.v1.jemartel.testnet"
signer_key_path = "/etc/verifier/verifier.ed25519"
account_id = "verifier-1.jemartel.testnet"

[nostr]
relays = ["wss://nostr-relay-production.up.railway.app/"]
nostr_key_hex = "<secp256k1-private-key-hex>"
# Verifier's Radicle DID (posted in events for discovery)
verifier_did = "did:key:z6Mkfoobar..."

[radicle]
node_home = "/var/lib/verifier/radicle"
seed = true  # always-on, acts as seed node

[executor]
runtime = "docker"  # docker | podman
default_image = "python:3.12-slim"
max_timeout_seconds = 3600
max_memory_mb = 4096
default_cpu_cores = 2
network_enabled = false
work_dir = "/var/lib/verifier/work"

[executor.images]
python = "python:3.12-slim"
rust = "rust:1.75-slim"
node = "node:20-slim"

[database]
path = "/var/lib/verifier/verifier.db"

[logging]
level = "info"
file = "/var/log/verifier/verifier.log"
```

## Deployment

```bash
# Single binary deployment
curl -sSL https://releases.near-escrow.dev/verifier/latest/verifier-linux-amd64 \
  -o /usr/local/bin/verifier
chmod +x /usr/local/bin/verifier

# Generate NEAR signer key
verifier keygen --account verifier-1.jemartel.testnet

# Initialize Radicle identity
verifier radicle-init --alias verifier-1

# Register with escrow contract
verifier register --contract escrow.v1.jemartel.testnet

# Run
verifier serve --config /etc/verifier/verifier.toml
```

Or Docker:
```bash
docker run -d \
  --name verifier \
  -v /etc/verifier:/etc/verifier:ro \
  -v /var/lib/verifier:/var/lib/verifier \
  near-escrow/verifier:latest \
  serve --config /etc/verifier/verifier.toml
```

## Security Model

### Trust Boundaries

```
Trusted:                    Untrusted:
  • Verifier service          • Worker code (runs in sandbox)
  • NEAR contract             • Worker output (validated by verify.sh)
  • Radicle protocol          • LLM judge (can be gamed)
  • Nostr event signatures    • External network (disabled in sandbox)
  • Agent input (in repo)
```

### Sandbox Guarantees

Worker execution containers:
- No network access (default)
- Read-only filesystem (except /output)
- CPU and memory limits enforced by cgroups
- Timeout kills the process
- No access to host filesystem
- No access to verifier signing keys
- No access to NEAR credentials

### Key Management

- ed25519 key pair for NEAR verdict signing (separate from NEAR account key)
- Radicle identity key (for P2P authentication, DID = did:key:...)
- Nostr secp256k1 key (for event signing, separate from Radicle and NEAR keys)
- All keys stored in `/etc/verifier/keys/`, readable only by verifier user

Three separate keys, three separate concerns:
- NEAR key = who gets paid
- Radicle key = who can access repos
- Nostr key = who posts events

## Contract Changes (Minimal)

Add one field to the Escrow struct:

```rust
/// Radicle Repository ID for the task repo
task_repo_rid: Option<String>,  // e.g. "rad:z4Ybyw4fybeProB6iQx6AVbESsdYn"
```

The contract still doesn't know or care what's in the repo. It's just a reference. The verifier reads the repo, runs verification, and submits a signed verdict. The contract checks the signature and settles.

## Multiple Verifiers

Anyone can run a verifier service. The agent chooses which verifier to use when creating the escrow (via `verifier_did` tag in Nostr + allow-list in Radicle repo). Verifiers compete on:

- **Speed** — how fast they verify
- **Reliability** — uptime, retry handling
- **Price** — verifier_fee_bps (basis points of escrow amount)
- **Trust** — reputation score (accuracy history)

Workers discover verifiers via Nostr (kind 41000 events include verifier_did). They clone from any verifier node seeding the repo.

## Future: Splitting Host and Verifier

In v2, hosting and verification could be separate roles:

- **Seeder nodes** — just host Radicle repos, earn tiny hosting fees
- **Verifier nodes** — run verification, earn verification fees
- **Workers** — do the actual task work

But for v1, the verifier does everything: host, verify, sign, earn the full fee. Simple.

## Resolved Design Decisions

| Question | Decision |
|----------|----------|
| Radicle DIDs ↔ NEAR accounts | Don't map. Separate systems, linked by escrow. |
| Worker authentication | NEAR signature → Radicle access (verifier adds DID to allow-list after on-chain claim) |
| Repo cleanup | 30-day TTL after settlement. Logs kept indefinitely in SQLite. |
| Disputes | None v1. Timeouts are the escape hatch. Pick the right verification method. |
| LLM judge model | Verifier-internal. Agent specifies model in MANIFEST.json. |
| Large files | External URLs in input/, cap repos at 100MB. |
| Rate limiting | Storage deposit + worker stake already in contract. Economic disincentive. |
| Coordination | Nostr only. No REST API. |
| Verifier discovery | Nostr kind 41000 events (verifier_did tag). |
| Result provenance | commit_sha in kind 41002 tags links on-chain result to git state. |
