# Escrow Agent Marketplace — Project Plan

## Overview
Trustless agent-to-agent marketplace. Agents post tasks on Nostr (signed via msig), workers execute them, LLM verifier scores the work. Built on NEAR with yield/resume for async verification.

## Architecture

```
Agent signs CreateEscrow action (ed25519 key) → posts to Nostr (kind 41000)
  ↓
Relayer picks up → extracts signed action → calls msig.execute()
  ↓
msig verifies ed25519 sig → calls escrow.create_escrow() (PendingFunding)
  ↓
Agent signs FundEscrow action → posts to Nostr (kind 41003)
  ↓
Relayer picks up → calls msig.execute() → msig calls ft_transfer_call()
  ↓
Escrow ft_on_transfer() sees sender=msig → Open
  ↓
Worker sees task (Nostr) → waits for Open → claims directly on escrow (InProgress)
  ↓
Worker does job → submit_result() → YIELDS (Verifying)
  ↓
LLM Verifier scores work → resume_verification(data_id_hex, verdict)
  ↓
verification_callback → _settle_escrow() → settle_callback()
  ↓
Worker paid OR agent refunded (to msig), verifier fee paid, storage refunded
```

## Key Architectural Decisions

1. **Two-key model**: Agent has secp256k1 key (Nostr identity) + ed25519 key (msig authorization). No cross-curve derivation — keys stored together in config, binding is operational.
2. **Relayer is a dumb pipe**: Relayer only extracts signed actions from Nostr events and submits to msig.execute(). Cannot fake, steal, or modify actions.
3. **Worker goes direct**: Workers claim and submit results directly on the escrow contract. They don't go through msig — only agents do.
4. **Nostr event kinds**: 41000 (task + CreateEscrow), 41001 (claim), 41002 (result), 41003 (generic actions: fund, cancel, withdraw, rotate).
5. **Cross-contract failures**: msig increments nonce before dispatching. If escrow call fails, nonce is consumed but funds returned. Agent retries with nonce+1. Event emitted for off-chain monitoring.
6. **Worker msig deferred to v1+**: Workers use regular NEAR accounts for now.

## Components

### 1. NEAR Escrow Contract ✅ (done)
- Location: `src/lib.rs`
- Status: Builds clean, 15 unit tests passing

**Features:**
- Two-phase funding: `create_escrow` (PendingFunding) → `ft_on_transfer` (Open)
- `claim()` — worker takes job (agent can't claim own escrow)
- `submit_result()` — stores result, creates yield promise, emits `result_submitted` with data_id
- `resume_verification(data_id_hex, verdict)` — verifier delivers verdict, calls `promise_yield_resume`
- `verification_callback()` — resumed by yield, validates score consistency, chains settlement
- `_settle_escrow()` + `settle_callback()` — FT transfers batched via `.and()`, callback checks all results manually
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

### 2. Agent Multisig Contract ✅ (done)
- Location: `agent-msig/src/lib.rs`
- Status: Builds clean, 16 unit tests passing

**Features:**
- `execute(action_json, signature)` — verifies ed25519 signature, enforces nonce, dispatches action
- Actions: CreateEscrow, FundEscrow, CancelEscrow, RegisterToken, RotateKey, Withdraw
- `ft_on_transfer()` — accepts all incoming FT tokens (returns U128(0))
- `force_rotate()` — owner emergency key rotation after 7200 block (~24h) cooldown
- NEP-297 `action_executed` event on every execute() for off-chain observability
- Views: `get_agent_pubkey`, `get_agent_npub`, `get_nonce`, `get_escrow_contract`, `get_last_action_block`, `get_owner`

**Action JSON format:**
```json
{"nonce": 1, "action": {"type": "create_escrow", "job_id": "...", ...}}
```
Signed with ed25519 key. Contract parses AFTER signature verification.

### 3. LLM Verifier Service ✅ (done)
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

### 4. Nostr Integration ✅ (done)
- Location: `nostr/`

**Files:**
- `relayer.py` — Watches for kind 41000 (task) and 41003 (generic action) events. Extracts signed actions from Nostr event tags, calls `msig.execute()` on-chain. Does NOT create escrows directly.
- `worker.py` — Watches for kind 41000 events, filters by capabilities, polls until escrow is Open (handles race condition with relayer), claims on escrow directly, executes task, submits result.
- `post_task.py` — CLI tool to post kind 41000 events. Agent queries msig nonce, builds CreateEscrow action, signs with ed25519 key, embeds in Nostr event tags.
- `sign_action.py` — CLI tool to post kind 41003 events for generic actions (fund, cancel, withdraw, rotate, register_token).
- `event_schema.json` — Event kind definitions (41000-41003) with required/optional tags.
- `requirements.txt` — Python deps (near-api-py, websockets, PyNaCl, base58, nostr-sdk)

**Two-key model:**
- `--nostr-key`: secp256k1 private key (hex) for Nostr event signing (identity)
- `--agent-key`: ed25519 private key (ed25519:base58...) for msig action signing (authorization)

### 5. Test Suite ✅ (done)
- Escrow contract: `src/tests.rs` — 15 tests
- Msig contract: `agent-msig/src/lib.rs` (inline) — 16 tests
- Total: 31 tests, all passing

## Settlement Logic

Settlement uses `.and()` to batch FT transfers in parallel, then `.then(settle_callback)`. The callback manually checks all promise results.

- **Passed** (score ≥ threshold): worker payout (amount - verifier_fee) + owner fee transfer, batched via `.and()`. All must succeed.
- **Failed** (score < threshold): agent refund (amount - verifier_fee) + owner fee transfer, batched via `.and()`.
- **Timeout** (~200 blocks): full refund to agent (single transfer, no verifier fee).
- **Any FT transfer fails** → `SettlementFailed` status. Owner retries via `retry_settlement()`.
- **Stuck escrow recovery**: If `verification_callback` partially committed before settlement (data_id cleared, status still Verifying), `retry_settlement` accepts `Verifying` with `settlement_target` set.
- Storage deposit (1 NEAR) always refunded to agent on final state.

## Bugs Found and Fixed

### Escrow contract bugs

1. **settle_callback only saw one transfer result (4 iterations)** — Fixed by manually iterating `promise_results_count()` + `promise_result_checked()`. See PLAN.md history for full details.

2. **EVENT_JSON prefix missing** — `emit_event()` logged raw JSON. NEP-297 requires `EVENT_JSON:` prefix.

3. **Stale doc comment**: `_settle_escrow` said "Uses Promise::all()" but uses `.and()`.

### Verifier bugs

4. **near_client double-encoding** — Passed pre-serialized bytes instead of dict.
5. **get_stats string vs bytes** — Passed Python `""` instead of `b""`.
6. **Signer init wrong** — Passed raw string instead of `KeyPair(key_str)`.
7. **Unbounded processed set** — Capped at 10k entries.

### Msig + Nostr bugs (msig-v2 rewrite)

8. **msig arg name mismatch** — `_create_escrow` sent `"token_contract"` / `"description"` but escrow expects `"token"` / `"task_description"`.
9. **msig ft_on_transfer return type** — Returned `String` instead of `U128` (NEP-141 violation).
10. **Relayer called escrow directly** — Rewritten to route through `msig.execute()` so `escrow.agent = msig` (correct identity chain).
11. **Nostr schema missing npub/action/action_sig tags** — Added to event_schema.json.
12. **No kind 41003 schema** — Added for generic actions.

### Worker bugs (msig-v2 compatibility)

13. **Race condition** — Worker checked escrow before relayer created it. Fixed with polling retry (`_wait_for_open`).
14. **Config nesting mismatch** — Worker read top-level keys but config has `worker: {...}` section. Fixed to check both.
15. **max_reward not enforced** — Config had max_reward but never compared against task reward. Fixed.
16. **skills tag parsing** — Dict approach lost all but last `skills` tag. Fixed with `parse_multi_tags()`.

## File Structure
```
near-escrow/
├── src/
│   ├── lib.rs           # Escrow contract ✅
│   └── tests.rs         # 15 unit tests ✅
├── agent-msig/
│   ├── src/
│   │   └── lib.rs       # Msig contract + 16 tests ✅
│   └── Cargo.toml
├── verifier/            # LLM verifier service ✅
│   ├── main.py          # Event loop
│   ├── scorer.py        # Multi-pass Gemini scoring
│   ├── near_client.py   # NEAR RPC client
│   ├── config.example.json
│   ├── requirements.txt
│   └── .gitignore
├── nostr/               # Nostr integration ✅
│   ├── relayer.py       # Nostr → msig.execute() bridge
│   ├── worker.py        # Worker agent (claim + execute + submit)
│   ├── post_task.py     # CLI: post tasks with signed CreateEscrow
│   ├── sign_action.py   # CLI: sign/post generic msig actions
│   ├── event_schema.json # Event kind definitions (41000-41003)
│   ├── config.example.json
│   ├── requirements.txt
│   └── .gitignore
├── scripts/             # 🔲 Deploy scripts
│   ├── deploy_testnet.sh
│   └── deploy_mainnet.sh
├── Cargo.toml
├── PLAN.md
├── DESIGN-MSIG.md
└── README.md
```

## Dependencies
- near-sdk 5.6+ (yield/resume API)
- google-genai (Gemini scoring)
- near-api-py (NEAR RPC — verifier, relayer, worker)
- PyNaCl + base58 (ed25519 signing — post_task, sign_action)
- nostr-sdk (Nostr event creation/posting)
- websockets (relay subscriptions — relayer, worker)

## Next Steps
1. ~~Write verifier service~~ ✅
2. ~~Write test suite~~ ✅
3. ~~Fix settlement callback bug~~ ✅
4. ~~Fix EVENT_JSON prefix~~ ✅
5. ~~Build Nostr integration~~ ✅
6. ~~Build msig contract + Nostr rewrite~~ ✅
7. ~~Fix worker.py for msig-v2~~ ✅
8. ~~Update docs (PLAN.md, DESIGN-MSIG.md)~~ ✅
9. Deploy to testnet (deploy scripts)
10. End-to-end test on testnet
11. Deploy to mainnet
