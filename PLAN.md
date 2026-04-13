# Escrow Agent Marketplace — Project Plan

## Overview
Trustless agent-to-agent marketplace. Agents post tasks on Nostr, workers execute them, LLM verifier scores the work. Built on NEAR with yield/resume for async verification.

## Architecture

```
Agent posts task on Nostr (kind:41000)
  ↓
Relayer picks up → create_escrow() on-chain (PendingFunding)
  ↓
Agent funds via ft_transfer_call → ft_on_transfer() (Open)
  ↓
Worker sees task (Nostr/FastNear) → claim() (InProgress)
  ↓
Worker does job → submit_result() → YIELDS (Verifying)
  ↓
LLM Verifier scores work → resume_verification(data_id_hex, verdict)
  ↓
verification_callback → _settle_escrow() → settle_callback()
  ↓
Worker paid OR agent refunded, verifier fee paid, storage refunded
```

## Components

### 1. NEAR Escrow Contract ✅ (done)
- Location: `src/lib.rs`
- Status: Builds clean (zero warnings), 15 unit tests passing

**Features:**
- Two-phase funding: `create_escrow` (PendingFunding) → `ft_on_transfer` (Open)
- `claim()` — worker takes job (agent can't claim own escrow)
- `submit_result()` — stores result, creates yield promise, emits `result_submitted` with data_id
- `resume_verification(data_id_hex, verdict)` — verifier delivers verdict, calls `promise_yield_resume`
- `verification_callback()` — resumed by yield, validates score consistency, chains settlement
- `_settle_escrow()` + `settle_callback()` — FT transfers via `.and()` batch + `#[callback_vec]`
- `cancel()` — PendingFunding (storage refund) or Open (FT refund via settlement)
- `refund_expired()` — anyone can call after timeout (blocked during Verifying)
- `retry_settlement()` — owner can retry failed FT transfers
- Storage deposit (1 NEAR) refunded on settle/cancel
- `EscrowView` hides internal fields (data_id, settlement_target)
- Paginated views: `list_open`, `list_by_agent`, `list_by_worker`, `list_verifying`
- Events use NEP-297 `EVENT_JSON:` prefix for indexer compatibility

**Contract state machine:**
```
PendingFunding → Open → InProgress → Verifying → Claimed
     ↓              ↓                              ↓
  Cancelled     Cancelled                      Refunded
                                                  ↓
                                          SettlementFailed → (retry)
```

### 2. LLM Verifier Service ✅ (done)
- Location: `verifier/`
- Status: Complete — main loop, scorer, NEAR client

**Files:**
- `main.py` — Event loop. Polls `list_verifying()` view, scores work, delivers verdict via `resume_verification`. Bounded processed set (10k cap).
- `scorer.py` — Multi-pass Gemini scoring. 4 independent passes at varying temperatures, median aggregation, 0-100 scale.
- `near_client.py` — NEAR RPC client. `get_escrow()`, `get_verifying_escrows()`, `resume_yield()`, `get_stats()`.
- `config.example.json` — Configuration template (network, contract_id, gemini_model, poll_interval, etc.)
- `requirements.txt` — Python deps (`google-genai`, `near-api-py`)

**Config:**
- `GEMINI_API_KEY` env var for Gemini auth
- `NEAR_VERIFIER_KEY` env var (ed25519:base58key) — creates `KeyPair` → `Signer`
- Default: gemini-2.5-flash, 4 passes, threshold 80, poll interval 3s

### 3. Test Suite ✅ (done)
- Location: `src/tests.rs`
- Status: 15 tests, all passing

**Coverage:**
- Contract init
- create_escrow: happy path + no deposit + duplicate ID + fee too high
- ft_on_transfer: funding + wrong sender + wrong amount
- claim: happy path + agent self-claim
- cancel: happy path + wrong caller
- Views: stats, storage deposit, list_open empty

### 4. Nostr Task Discovery 🔲 (TODO)
- Agent posts task as Nostr event
- Workers subscribe to task feed

**Event schema:**
```
kind: 41000 (agent task)
tags:
  - ["job_id", "<unique-id>"]
  - ["reward", "<amount>", "<token-contract>"]
  - ["timeout", "<hours>"]
  - ["verifier_fee", "<amount>"]
  - ["score_threshold", "<0-100>"]
  - ["escrow_contract", "<account.near>"]
  - ["agent", "<account.near>"]
content: task description + criteria (JSON or natural language)
```

### 5. Relayer 🔲 (TODO)
- Bridges Nostr events → on-chain
- Watches kind:41000 events
- Calls `create_escrow()` on behalf of agent (agent signs intent via Nostr)
- Can leverage existing Nostr→NEAR bridge (layerd port 7201→7203)
- Agent then funds via `ft_transfer_call`

### 6. Worker Agent 🔲 (TODO)
- Subscribes to Nostr task feed (kind:41000)
- Evaluates if task matches capabilities
- Calls `claim()` on-chain
- Does the work
- Calls `submit_result()` on-chain
- Waits for verification → payout

### 7. Agent Identity 🔲 (TODO)
- How agents link NEAR account ↔ Nostr keypair
- Options: Nostr event with NEAR signature, or NEAR social profile with npub
- Needed so workers can verify who posted the task

### 8. Deploy Scripts 🔲 (TODO)
- Testnet deployment script (cargo-near deploy)
- Mainnet deployment script
- Contract initialization (owner setup)

## Bugs Found and Fixed
1. **settle_callback blind spot** — `.then()` chain only captured last FT transfer result. Worker payout could fail silently. Fixed with `.and()` batch + `#[callback_vec]`.
2. **EVENT_JSON missing** — events logged without NEP-297 `EVENT_JSON:` prefix. Indexers couldn't detect them.
3. **near_client double-encoding** — `function_call()` already serializes args, passing pre-serialized bytes caused double-encoding.
4. **get_stats string vs bytes** — `view_call` expects bytes, not Python string.
5. **Signer init** — `Signer()` expects `KeyPair` object, not raw key string.

## Settlement Logic
- **Passed** (score ≥ threshold): worker gets amount - verifier_fee, owner gets fee
- **Failed** (score < threshold): agent gets refund - verifier_fee, owner gets fee
- **Timeout** (~200 blocks): full refund to agent, no verifier fee
- **SettlementFailed**: callback panic reverts state, admin retries via `retry_settlement()`
- Storage deposit (1 NEAR) always refunded to agent on final state

## Key Design Decisions
- Verifier is OFF-CHAIN LLM service, not WASM
- yield/resume pattern for async verification (~200 block timeout)
- Verifier gets paid even on failure (scoring costs compute)
- No verifier allowlist — anyone can call `resume_verification` (off-chain trust model)
- Relayer/Nostr is just discovery — contract doesn't know about it
- Two-phase funding prevents stuck FT tokens
- Score consistency enforced on-chain (can't fake passed with low score)
- Internal state (data_id, settlement_target) never exposed in views
- `.and()` + `#[callback_vec]` for settlement: all transfers must succeed or state reverts

## Dependencies
- near-sdk 5.6+ (yield/resume API)
- google-genai (Gemini scoring)
- near-api-py (verifier NEAR RPC)
- Nostr relays (task discovery — TODO)
- FastNear RPC (chain queries)

## File Structure
```
near-escrow/
├── src/
│   ├── lib.rs           # Contract ✅
│   └── tests.rs         # 15 unit tests ✅
├── verifier/            # LLM verifier service ✅
│   ├── main.py          # Event loop
│   ├── scorer.py        # Multi-pass Gemini scoring
│   ├── near_client.py   # NEAR RPC client
│   ├── config.example.json
│   ├── requirements.txt
│   └── .gitignore
├── Cargo.toml           # Rust deps
├── PLAN.md              # This file
├── README.md
├── scripts/             # 🔲 Deploy scripts
│   ├── deploy_testnet.sh
│   └── deploy_mainnet.sh
└── nostr/               # 🔲 Nostr integration
    ├── event_schema.json
    ├── relayer.py
    └── worker.py
```

## Next Steps
1. ~~Write verifier service~~ ✅
2. ~~Write test suite~~ ✅
3. ~~Fix settlement callback bug~~ ✅
4. ~~Fix EVENT_JSON prefix~~ ✅
5. Deploy to testnet (deploy scripts)
6. Build Nostr integration (relayer + worker)
7. End-to-end test on testnet
8. Deploy to mainnet
