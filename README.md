# NEAR Escrow Marketplace

On-chain escrow protocol for AI agent task marketplace with Nostr-based off-chain coordination.

## Architecture

```
Agent (posts task)          Worker (does work)         Verifier (scores)
     │                            │                          │
     ├─ kind 41000 ──► Nostr ────┤                          │
     │                  Relay     │                          │
     │                            ├─ kind 41002 ──► Nostr    │
     │                            │                  Relay   │
     │                            │                          │
     ▼                            ▼                          ▼
  Relayer ◄──────────────────── Daemon ◄─────────────── Verifier
  (Python)                      (Rust)                 (Python/Gemini)
     │                            │                          │
     └── msig.execute() ──► NEAR Escrow Contract ◄──────────┘
                              (state machine)
```

## Contracts

| Contract | Location | Purpose |
|----------|----------|---------|
| **Escrow** | `src/` | Payment arbiter, state machine, verification |
| **Agent Msig** | `agent-msig/` | Ed25519 + Nostr auth, spending limits |
| **Clear Msig** | `../nostr-msig/` | Schnorr governance multisig |

## Escrow Lifecycle

```
PendingFunding → Open → InProgress → Verifying → Claimed (worker paid)
                                        ↘ Refunded (agent refund)
```

1. **Create** — Agent creates escrow with task description, criteria, reward
2. **Fund** — FT tokens deposited via `ft_transfer_call` or `FundEscrow`
3. **Claim** — Worker stakes and claims the task
4. **Submit** — Worker submits result (optionally stored in FastNear KV)
5. **Verify** — Verifier scores output with LLM (multi-verifier consensus supported)
6. **Settle** — Pass: worker paid. Fail: agent refunded.

## Quick Start

### Build

```bash
# Escrow contract
cargo build --release --target wasm32-unknown-unknown

# Agent msig
cargo build --release --target wasm32-unknown-unknown -p agent-msig

# Optimized WASM (requires wasm-opt)
bash ../nostr-msig/build.sh
```

### Deploy (testnet)

```bash
near contract deploy <account> use-file target/wasm32-unknown-unknown/release/near_escrow.wasm \
  without-init-call network-config testnet sign-with-access-key-file <key.json> send

near contract call-function as-transaction <account> new json-args '{
  "verifier_set": [{"account_id": "verifier.test.near", "public_key": "<hex>", "active": true}],
  "consensus_threshold": 1,
  "allowed_tokens": []
}' prepaid-gas '30 Tgas' attached-deposit '0 NEAR' sign-as <account> \
  network-config testnet sign-with-access-key-file <key.json> send
```

## Testing

### Unit / Sandbox Tests (95 tests)

```bash
# All integration tests
cargo test -p integration-tests

# E2E sandbox tests
cargo test -p integration-tests --test e2e-sandbox

# Specific test
cargo test -p integration-tests --test integration -- test_full_happy_path
```

### E2E Tests (6 tests)

Full local loop: sandbox + daemon + Nostr relay, no testnet needed.

```bash
cargo test -p integration-tests --test e2e-sandbox
```

| Test | What it proves |
|------|---------------|
| `test_e2e_happy_path` | Full escrow lifecycle on sandbox — worker gets paid |
| `test_e2e_verification_failure` | Verification fails → agent refunded |
| `test_e2e_rpc_endpoint_exposed` | Sandbox exposes real HTTP JSON-RPC |
| `test_e2e_nostr_round_trip` | Kind 41000 event reaches real Nostr relay |
| `test_e2e_daemon_connects_to_sandbox` | Daemon relayer connects to sandbox RPC |
| `test_e2e_full_local_daemon_flow` | **Full loop: Nostr → Daemon → Sandbox** |

### Test Coverage

**95 sandbox tests** covering every public method:

- Create/fund/claim/submit/verify/settle flows
- Worker registration, linking, pause/unpause, withdrawal (NEAR + FT)
- Owner transfer, pause/unpause, storage deposit config
- Multi-verifier consensus (2-of-3 threshold)
- FT funding via `ft_transfer_call`
- Admin: verifier management, token allowlist, consensus threshold
- Views: list_by_status, list_by_agent, list_by_worker, get_stats

**150 daemon tests** in `near-inlayer/worker/`

### Known Sandbox Limitations

2 tests are `#[ignore]` due to sandbox constraints:

- `test_force_cancel_verifying` — needs 172k blocks (48h) for safety timeout
- `test_flow_b_full_lifecycle` — sandbox nonce race (view/mutation see different state)

Both should work on testnet/mainnet where time passes naturally.

## Nostr Event Schema

| Kind | Name | Direction |
|------|------|-----------|
| 41000 | TaskPosted | Agent → Network |
| 41001 | TaskUpdated | Agent → Network |
| 41002 | ResultSubmitted | Worker → Network |
| 41003 | TaskFunded | Agent → Network |
| 41004 | TaskDispatched | Relayer → Network |
| 41005 | TaskConfirmed | Daemon → Network |

## Off-Chain Services

| Service | Language | Location |
|---------|----------|----------|
| Relayer | Python | `nostr/relayer.py` |
| Verifier | Python + Gemini | `verifier/` |
| Daemon | Rust | `../near-inlayer/worker/` |

### Relayer

Watches Nostr for kind 41000 events, submits CreateEscrow + FundEscrow via msig.execute(), publishes kind 41004.

### Verifier

Polls escrow for Verifying status, fetches results from FastNear KV, scores with Gemini, settles on-chain.

### Daemon

Rust binary with escrow client, relayer mode, verifier mode, supervisor. Dual-mode config (`direct` / `escrow` / `both`).

## Security Features

- **Reentrancy guard** — locked flag prevents re-entry during cross-contract calls
- **Emergency pause** — owner can pause all state-changing operations
- **Spending limits** — per-action and daily limits on agent msig
- **Worker stake** — workers must stake to claim tasks
- **Multi-verifier consensus** — configurable threshold (e.g., 2-of-3)
- **Proposal expiry** — expired proposals can't be executed
- **Force cancel** — owner can cancel stuck verifications after safety timeout

## Related

- [nostr-msig](../nostr-msig/) — Schnorr governance multisig (clear-msig)
- [near-inlayer](../near-inlayer/) — Daemon with escrow integration
- [PLAN.md](../PLAN.md) — Full integration plan
