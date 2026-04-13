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

### 4. Nostr Task Discovery ✅ (done)
- Event schema defined (`nostr/event_schema.json`)
- Three event kinds:
  - `41000` — Task posted by agent (tags: job_id, reward, timeout, agent, escrow)
  - `41001` — Worker claimed task
  - `41002` — Worker submitted result
- Content is JSON with `task_description` + `criteria`
- Tags for filtering: category, skills, priority

### 5. Relayer ✅ (done)
- Location: `nostr/relayer.py`
- WebSocket subscription to task events (kind 41000)
- Creates on-chain escrow via `create_escrow()` when task detected
- Attaches 1 NEAR storage deposit
- Deduplication via processed event cache (10k cap)
- `--dry-run` mode for watching without creating escrows

### 6. Worker Agent ✅ (done)
- Location: `nostr/worker.py`
- Subscribes to task events, filters by capabilities
- Checks escrow is Open before claiming
- `claim()` → execute task → `submit_result()` on-chain
- `_execute_task()` is a placeholder — plug in real agent logic (LLM, code gen, etc.)
- Capability matching: checks task skills/category against config

### 7. Post Task Helper ✅ (done)
- Location: `nostr/post_task.py`
- CLI tool to post kind 41000 events to Nostr
- Signs with nostr-sdk, sends to configured relays

### 8. Agent Identity 🔲 (TODO)
- How agents link NEAR account ↔ Nostr keypair
- Options: Nostr event with NEAR signature, or NEAR social profile with npub
- Needed so workers can verify who posted the task

### 9. Deploy Scripts 🔲 (TODO)
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
- `.and()` + `#[callback_result]` for settlement: failures caught → SettlementFailed → retry_settlement

## Dependencies
- near-sdk 5.6+ (yield/resume API)
- google-genai (Gemini scoring)
- near-api-py (verifier NEAR RPC)
- Nostr relays (task discovery — TODO)
- FastNear RPC (chain queries — verifier polls `list_verifying()` view)

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
├── nostr/               # Nostr integration ✅
│   ├── relayer.py       # Nostr → on-chain bridge
│   ├── worker.py        # Worker agent (claim + execute + submit)
│   ├── post_task.py     # CLI tool to post tasks
│   ├── event_schema.json # Event kind definitions
│   ├── config.example.json
│   ├── requirements.txt
│   └── .gitignore
├── scripts/             # 🔲 Deploy scripts
│   ├── deploy_testnet.sh
│   └── deploy_mainnet.sh
├── Cargo.toml
├── PLAN.md
└── README.md
```

## Next Steps
1. ~~Write verifier service~~ ✅
2. ~~Write test suite~~ ✅
3. ~~Fix settlement callback bug~~ ✅ (uses `#[callback_result]`, `SettlementFailed` → `retry_settlement`)
4. ~~Fix EVENT_JSON prefix~~ ✅
5. ~~Fix pagination (filter before skip)~~ ✅
6. ~~Add `list_verifying()` view~~ ✅
7. ~~Update verifier to poll `list_verifying()` instead of block scanning~~ ✅
8. ~~Build Nostr integration (relayer + worker + post_task + schema)~~ ✅
9. Deploy to testnet (deploy scripts)
10. End-to-end test on testnet
11. Deploy to mainnet
