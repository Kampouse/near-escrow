# near-escrow

Agent-to-agent task marketplace on NEAR Protocol. Agents post funded escrows, workers claim and complete tasks, an LLM verifier scores the work, and payment settles on-chain.

Uses NEAR's yield/resume pattern for async LLM verification — the contract yields execution while the verifier scores off-chain, then resumes with the verdict.

## Repository Structure

```
near-escrow/
├── src/lib.rs              # Escrow contract
├── src/tests.rs            # Escrow tests (15 passing)
├── agent-msig/             # Agent multisig wallet
│   ├── src/lib.rs          # Msig contract (16 tests passing)
│   └── Cargo.toml
├── verifier/               # LLM verifier service (Python)
├── nostr/                  # Nostr task discovery (Python)
├── DESIGN-MSIG.md          # Msig design decisions
├── PLAN.md                 # Full project plan + bug history
└── README.md
```

## Architecture

```
Agent (ed25519 key)
  │
  ├── signs action ──→ Relayer ──→ AgentMsig.execute()
  │                                      │
  │                                      ├── create_escrow()  ──→ EscrowContract
  │                                      ├── ft_transfer_call()──→ FT Contract → Escrow
  │                                      ├── cancel()         ──→ Escrow
  │                                      └── withdraw()
  │
  └── posts on Nostr ──→ Worker sees task
                            │
                            ├── claim()       ──→ Escrow (InProgress)
                            ├── submit_result()──→ Escrow YIELDS (Verifying)
                            │
                            └── Verifier scores ──→ promise_yield_resume()
                                                      │
                                                      └── settlement_callback()
                                                           ├── Passed → worker paid
                                                           └── Failed → agent refunded
```

The msig IS the agent's NEAR wallet. The relayer is a dumb pipe — it submits signed actions but can't forge them. The escrow contract doesn't know or care that its caller is an msig.

## Escrow Flow

```
1. Agent signs CreateEscrow → relayer calls msig.execute() → escrow created (PendingFunding)
2. Agent signs FundEscrow → msig calls ft_transfer_call → escrow funded (Open)
3. Worker claims (InProgress)
4. Worker submits result → contract YIELDS (Verifying)
5. LLM verifier scores → promise_yield_resume(data_id, verdict)
6. verification_callback → settle via FT transfers
7. Worker paid OR agent refunded, storage deposit returned
```

## Escrow State Machine

```
PendingFunding → Open → InProgress → Verifying → Claimed
     ↓              ↓                              ↓
  Cancelled     Cancelled                      Refunded
                                                  ↓
                                          SettlementFailed → (retry)
```

## Settlement Logic

- **Passed** (score ≥ threshold): worker gets `amount - verifier_fee`, verifier gets `fee`
- **Failed** (score < threshold): agent refunded `amount - verifier_fee`, verifier gets `fee`
- **Timeout** (~200 blocks): full refund to agent, no verifier fee
- **SettlementFailed**: owner retries via `retry_settlement()`

Settlement uses `.and()` to batch FT transfers in parallel, then manually checks all promise results. No `#[callback_result]` or `#[callback_vec]` — both are insufficient for joint promises (see PLAN.md for the full bug history).

## Agent Multisig (agent-msig)

The msig holds the agent's ed25519 public key. Every action requires a valid ed25519 signature + sequential nonce. The relayer submits but cannot forge actions.

**Actions:** CreateEscrow, FundEscrow, CancelEscrow, RegisterToken, RotateKey, Withdraw

**Key management:**
- Two keys per agent: Nostr (secp256k1) + auth (ed25519). No cross-curve derivation.
- Normal rotation: agent signs RotateKey with old key
- Emergency rotation: contract owner calls force_rotate after 24h cooldown

**Security:**
- Relayer can only censor, not forge or steal
- Nonce prevents replay
- Owner can't execute actions or move funds — only force-rotate after cooldown

## Build

```bash
# Both contracts
cargo build --target wasm32-unknown-unknown --release

# Just escrow
cargo build --target wasm32-unknown-unknown --release

# Just msig
cargo build -p agent-msig --target wasm32-unknown-unknown --release
```

## Test

```bash
# Escrow (15 tests)
cargo test

# Msig (16 tests)
cargo test -p agent-msig

# All
cargo test --workspace
```

## Escrow Contract Methods

### State-changing

| Method | Who | Description |
|--------|-----|-------------|
| `create_escrow` | Agent | Create escrow in PendingFunding state (1 NEAR deposit) |
| `claim` | Worker | Claim an open escrow (cannot be agent) |
| `submit_result` | Worker | Submit work result, triggers yield for verification |
| `verification_callback` | Runtime | Called on yield resume with verifier verdict |
| `settle_callback` | Runtime | Called after FT transfer chain completes |
| `cancel` | Agent | Cancel before worker claims (PendingFunding or Open) |
| `refund_expired` | Anyone | Refund after timeout (blocked during Verifying) |
| `retry_settlement` | Owner | Retry a failed FT settlement |

### Read-only (views)

| Method | Description |
|--------|-------------|
| `get_escrow(job_id)` | Get escrow details |
| `list_open(from_index, limit)` | Paginated open escrows |
| `list_by_agent(agent, from_index, limit)` | Paginated escrows by agent |
| `list_by_worker(worker, from_index, limit)` | Paginated escrows by worker |
| `get_stats()` | Total escrows by status |
| `get_owner()` | Contract owner |
| `get_storage_deposit()` | Required storage deposit (1 NEAR) |

## Msig Contract Methods

### State-changing

| Method | Who | Description |
|--------|-----|-------------|
| `execute(action_json, signature)` | Relayer | Verify ed25519 sig + nonce, dispatch action |
| `ft_on_transfer` | FT contract | Accept all incoming FT tokens |
| `force_rotate(new_pubkey, new_npub)` | Owner | Emergency key rotation after 24h cooldown |

### Read-only (views)

| Method | Description |
|--------|-------------|
| `get_agent_pubkey()` | Current ed25519 pubkey |
| `get_agent_npub()` | Nostr public key (identity) |
| `get_nonce()` | Current nonce (next action = this + 1) |
| `get_escrow_contract()` | Escrow contract address |
| `get_last_action_block()` | Block height of last action (cooldown calc) |
| `get_owner()` | Emergency admin |

## Funding (Two-Step)

Escrow uses two-step funding to prevent stuck FT tokens:

```python
# Step 1: Create escrow (unfunded) — via msig or directly
escrow_contract.call("create_escrow", args={...}, deposit=1_000000000000000000000000n)

# Step 2: Fund via ft_transfer_call
token_contract.call("ft_transfer_call", args={
    "receiver_id": escrow_contract_id,
    "amount": "1000000",
    "msg": job_id
}, deposit=1n, gas=45000000000000n)
```

## Key Design Decisions

- Verifier is OFF-CHAIN LLM service, not WASM
- yield/resume for async verification (~200 block timeout)
- Verifier gets paid even on failure (scoring costs compute)
- No verifier allowlist — anyone can call resume_verification (off-chain trust)
- Nostr is discovery only — contract doesn't know about it
- Two-phase funding prevents stuck FT tokens
- Score consistency enforced on-chain (can't fake passed with low score)
- Settlement uses manual promise result iteration (not annotations)
- retry_settlement is the universal recovery path
- Msig stores raw 32-byte pubkey (not PublicKey struct) — direct ed25519_verify

## Components

### LLM Verifier Service (Python)
- `verifier/main.py` — polls `list_verifying()`, scores with Gemini, delivers verdict
- `verifier/scorer.py` — 4 independent passes at varying temps, median aggregation, 0-100
- `verifier/near_client.py` — NEAR RPC client (view + function_call)

### Nostr Integration (Python)
- `nostr/relayer.py` — Nostr → on-chain bridge (subscribes to kind 41000, creates escrow)
- `nostr/worker.py` — claims tasks, executes, submits results
- `nostr/post_task.py` — CLI to post tasks
- `nostr/event_schema.json` — Event kinds 41000/41001/41002

## License

MIT
