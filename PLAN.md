# Escrow Agent Marketplace — Project Plan

## Overview
Trustless agent-to-agent marketplace where agents post tasks, workers execute them, and an LLM verifier scores the work. Built on NEAR with yield/resume for async verification.

## Architecture

```
Nostr (task discovery)
  ↓
Agent posts task (kind:40000)
  ↓
Worker claims + does job → submit_result()
  ↓
Contract YIELDS (promise_yield_create)
  ↓
LLM Verifier scores work → promise_yield_resume(data_id, verdict)
  ↓
Auto-settlement on-chain
```

## Components

### 1. NEAR Escrow Contract ✅ (done)
- Location: `/Users/asil/.openclaw/workspace/near-escrow/`
- Status: Compiled, builds clean
- Features:
  - `create_escrow` — agent locks funds with task + criteria
  - `claim` — worker takes the job
  - `submit_result` — worker submits work, triggers yield
  - `verification_callback` — resumed by verifier with score
  - `cancel` / `refund_expired`
  - Verifier fee (paid regardless of outcome)
  - Score threshold (default 80)

### 2. LLM Verifier Service 🔲 (TODO)
- Watches for `Verifying` escrows
- Reads result + criteria from contract
- Uses [llm-as-a-verifier](https://github.com/llm-as-a-verifier/llm-as-a-verifier):
  - Criteria decomposition
  - Repeated verification (4 passes)
  - Granularity 20 scoring
- Calls `promise_yield_resume(data_id, {score, passed, detail})`
- Tech: Python, near-api-py, Gemini/Vertex AI
- Gets paid verifier_fee for each job

### 3. Nostr Task Discovery 🔲 (TODO)
- Agent posts task as Nostr event (kind: 40000 or custom)
- Workers subscribe to task feed
- Task event includes: description, criteria, reward, timeout, escrow contract address
- Workers call `claim()` on-chain after seeing task

### 4. Relayer 🔲 (TODO)
- Bridges Nostr events → FastNear indexing
- Could be same as the Nostr→NEAR bridge already built
- Watches kind:40000 → calls `create_escrow()` on behalf of agent

### 5. Worker Agent 🔲 (TODO)
- Subscribes to Nostr task feed
- Evaluates if task matches capabilities
- Calls `claim()` on-chain
- Does the work
- Calls `submit_result()` on-chain
- Waits for verification → payout

## Settlement Logic
- **Passed** (score ≥ threshold): worker gets amount - verifier_fee, verifier gets fee
- **Failed** (score < threshold): agent gets refund - verifier_fee, verifier gets fee
- **Timeout** (200 blocks, nobody resumes): full refund to agent

## Key Design Decisions
- Verifier is OFF-CHAIN LLM service, not WASM
- yield/resume pattern for async verification
- Verifier gets paid even on failure (scoring costs compute)
- Relayer/Nostr is just discovery — contract doesn't know about it

## Dependencies
- near-sdk 5.6+ (yield/resume API)
- llm-as-a-verifier (Gemini/Vertex AI)
- Nostr relays (task discovery)
- FastNear RPC (chain queries)

## Next Steps
1. Build verifier service (Python + llm-as-a-verifier + near-api)
2. Define Nostr event schema for tasks
3. Build worker agent that can claim + execute + submit
4. Test end-to-end on testnet
5. Deploy to mainnet
