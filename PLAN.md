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
LLM Verifier scores work → promise_yield_resume(data_id, verdict)
  ↓
verification_callback → _settle_escrow() → settle_callback()
  ↓
Worker paid OR agent refunded, verifier fee paid, storage refunded
```

## Components

### 1. NEAR Escrow Contract ✅ (done)
- Location: `/Users/asil/.openclaw/workspace/near-escrow/`
- Status: Compiled, builds clean

**Features:**
- Two-phase funding: `create_escrow` (PendingFunding) → `ft_on_transfer` (Open)
- `claim()` — worker takes job (agent can't claim own escrow)
- `submit_result()` — stores result, creates yield promise
- `verification_callback()` — resumed by verifier, validates score consistency
- `_settle_escrow()` + `settle_callback()` — FT transfers with error handling
- `cancel()` — PendingFunding (storage refund) or Open (FT refund via settlement)
- `refund_expired()` — anyone can call after timeout (blocked during Verifying)
- `retry_settlement()` — owner can retry failed FT transfers
- Storage deposit (1 NEAR) refunded on settle/cancel
- `EscrowView` hides internal fields (data_id, settlement_target)
- Paginated views (list_open, list_by_agent, list_by_worker)

**Contract state machine:**
```
PendingFunding → Open → InProgress → Verifying → Claimed
     ↓              ↓                              ↓
  Cancelled     Cancelled                      Refunded
                                                  ↓
                                          SettlementFailed → (retry)
```

### 2. LLM Verifier Service 🔲 (TODO)
- Watches for `Verifying` escrows (poll or event listener)
- Reads result + criteria from contract view
- Uses [llm-as-a-verifier](https://github.com/llm-as-a-verifier/llm-as-a-verifier):
  - Criteria decomposition (break task into checkable parts)
  - Repeated verification (4 passes for reliability)
  - Granularity 20 scoring (0-100)
- Calls `promise_yield_resume(data_id, {score, passed, detail})` on contract
- Gets paid verifier_fee for each job (regardless of outcome)
- Tech: Python, near-api-py or json-rpc, Gemini/Vertex AI

**Key detail:** `promise_yield_resume` is a standalone function call. The verifier service needs the data_id (from `result_submitted` event) and a funded NEAR account to make the call.

### 3. Nostr Task Discovery 🔲 (TODO)
- Agent posts task as Nostr event
- Workers subscribe to task feed

**Event schema (to be defined):**
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

### 4. Relayer 🔲 (TODO)
- Bridges Nostr events → on-chain
- Watches kind:41000 events
- Calls `create_escrow()` on behalf of agent (agent signs intent via Nostr)
- Can leverage existing Nostr→NEAR bridge (layerd port 7201→7203)
- Agent then funds via `ft_transfer_call`

### 5. Worker Agent 🔲 (TODO)
- Subscribes to Nostr task feed (kind:41000)
- Evaluates if task matches capabilities
- Calls `claim()` on-chain
- Does the work
- Calls `submit_result()` on-chain
- Waits for verification → payout

### 6. Agent Identity 🔲 (TODO)
- How agents link NEAR account ↔ Nostr keypair
- Options: Nostr event with NEAR signature, or NEAR social profile with npub
- Needed so workers can verify who posted the task

### 7. Test Suite 🔲 (TODO)
- Unit tests for contract (cargo test with near-sdk test_utils)
- Sim tests for yield/resume flow
- Integration test: full create → fund → claim → submit → verify → settle

### 8. Deploy Scripts 🔲 (TODO)
- Testnet deployment script (cargo-near deploy)
- Mainnet deployment script
- Contract initialization (owner setup)

## Settlement Logic
- **Passed** (score ≥ threshold): worker gets amount - verifier_fee, verifier gets fee
- **Failed** (score < threshold): agent gets refund - verifier_fee, verifier gets fee
- **Timeout** (200 blocks, nobody resumes): full refund to agent, no verifier fee
- **SettlementFailed**: owner retries via `retry_settlement()`
- Storage deposit (1 NEAR) always refunded to agent on final state

## Key Design Decisions
- Verifier is OFF-CHAIN LLM service, not WASM
- yield/resume pattern for async verification (200 block timeout)
- Verifier gets paid even on failure (scoring costs compute)
- Relayer/Nostr is just discovery — contract doesn't know about it
- Two-phase funding prevents stuck FT tokens
- Score consistency enforced on-chain (can't fake passed with low score)
- Internal state (data_id, settlement_target) never exposed in views

## Dependencies
- near-sdk 5.6+ (yield/resume API)
- llm-as-a-verifier (Gemini/Vertex AI)
- Nostr relays (task discovery)
- FastNear RPC (chain queries)
- Python near-api-py (verifier service)

## File Structure
```
near-escrow/
├── src/lib.rs           # Contract (done)
├── Cargo.toml           # Rust deps (done)
├── PLAN.md              # This file
├── tests/               # 🔲 Test suite
├── scripts/
│   ├── deploy_testnet.sh  # 🔲
│   └── deploy_mainnet.sh  # 🔲
├── verifier/            # 🔲 LLM verifier service
│   ├── main.py
│   ├── scorer.py        # Wraps llm-as-a-verifier
│   └── near_client.py   # promise_yield_resume calls
└── nostr/               # 🔲 Nostr integration
    ├── event_schema.json
    ├── relayer.py
    └── worker.py
```

## Next Steps
1. Update PLAN.md ✅
2. Define Nostr event schema
3. Build verifier service (Python + llm-as-a-verifier + NEAR RPC)
4. Write test suite
5. Deploy to testnet
6. Build relayer + worker agent
7. End-to-end test on testnet
8. Deploy to mainnet
