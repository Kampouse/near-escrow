# Escrow + Inlayer Merge Plan

## Why

Two half-systems that complete each other:

- **near-escrow** has payment/settlement (state machine, FT escrow, msig auth, verifier, Nostr discovery) but no execution — workers are stubs that sleep and return JSON.
- **near-inlayer** has the execution pipeline (daemon, worker routing, job-queue contract, Nostr coordination) but only advisory signaling — no escrow, no trustless settlement, no verifier.

Merged: inlayer's daemon becomes the plumbing layer in the escrow marketplace. Worker agents (each with their own msig) do the actual work. The daemon routes tasks, relays worker-signed claim/submit actions via msig.execute(), and handles KV writes. One protocol, one flow.

---

## Foundational Decisions

### 1. Kind Numbers: 41xxx namespace

Keep escrow's range, extend with execution lifecycle events. Inlayer's 72xx are retired.

| Kind | Name | Who posts | Purpose |
|------|------|-----------|---------|
| 41000 | Task | Agent | CreateEscrow signed action + task metadata |
|| 41001 | Claim | Daemon | Worker claimed (via worker_msig.execute) |
|| 41002 | Result | Worker Agent | Work result + signed claim/submit actions |
|| 41003 | Action | Agent | Generic msig action (fund, cancel, withdraw, rotate) |
|| 41004 | Dispatch | Daemon (relayer) | Escrow funded on-chain (FUNDED signal to workers) |
|| 41005 | Confirmed | Daemon | Settlement confirmed on-chain |

Rationale: 41000-41003 are already deployed in escrow's event_schema.json with full tag specs. Adding 41004/41005 for execution lifecycle is minimal extension. Inlayer's 7201-7205 had no adoption outside the daemon.

### 2. Identity: Two-key for all parties

**Agents (task posters):**
- secp256k1 key → Nostr identity (event signing, discovery)
- ed25519 key → msig authorization (on-chain actions verified by msig contract)
- Keys stored together in agent config. Binding is operational, not cryptographic.
- Relayer is a dumb pipe — can only extract and submit signed actions, cannot forge them.

**Workers (execution nodes):**
- secp256k1 key → Nostr identity (claim/result events)
- ed25519 key → msig authorization (on-chain claim + submit_result via msig.execute)
- Worker has its own msig — pre-signs claim() and submit_result() actions offline
- Posts kind 41002 with tags: worker_msig, claim_action, claim_sig, submit_action, submit_sig
- Daemon relays both actions via msig.execute() — worker's own funds at stake
- Settlement pays worker's msig directly — real on-chain identity, real reputation
- Worker never touches RPC or on-chain directly — all via Nostr + pre-signed actions
- Full worker protocol spec: [WORKER-SPEC.md](./WORKER-SPEC.md) — workers don't install daemon code, they just post Nostr events

### 3. Contracts: Separate, not merged

The escrow contract and inlayer contract stay separate. They serve different purposes:

- **Escrow contract** — payment arbiter. Holds FT tokens, manages state machine, settles via verifier verdict. Knows nothing about WASM or execution.
- **Inlayer contract** — job queue. Receives execution requests, holds payment for compute, workers resolve results. Knows nothing about escrow or verification.

The merge happens at the **daemon layer**, not the contract layer. The daemon coordinates between both contracts.

Why not merge contracts:
- Escrow is about trust between two parties (agent + worker). Inlayer is about compute delivery. Different security models.
- Escrow uses yield/resume for async LLM verification. Inlayer uses direct resolve. Different settlement patterns.
- Keeping them separate means each can be audited, tested, and upgraded independently.
- The daemon bridges them — that's the integration point.

### 4. FastNear KV: Core data layer

FastNear KV is the data store for work results. The escrow contract only holds a KV reference (~150 bytes). Full results live in KV. This is core to v1, not optional.

---

## Merged Flow

```
Agent signs CreateEscrow + FundEscrow actions (ed25519)
  → posts kind 41000 to Nostr (signed with secp256k1)
  → event contains both actions + signatures
      ↓
Relayer picks up 41000
  → extracts signed actions + signatures
  → calls msig.execute(create_action, sig) → msig calls escrow.create_escrow()
  → calls msig.execute(fund_action, sig) → msig calls ft_transfer_call()
  → escrow: PendingFunding → Open (one shot)
  → publishes kind 41004 (FUNDED) — signals workers escrow is ready to claim
      ↓
Worker agent sees 41004 on Nostr (escrow funded, safe to claim)
  → does the actual work off-chain
  → pre-signs claim() with worker msig key (stakes own funds)
  → pre-signs submit_result() with worker msig key (deterministic kv_reference)
  → posts kind 41002 (RESULT) to Nostr with tags:
      job_id, result/output, worker_msig,
      claim_action, claim_sig (64 bytes ed25519),
      submit_action, submit_sig (64 bytes ed25519)
      ↓
Daemon plumbing thread sees 41002
  → extracts worker_msig, signed actions, signatures from tags
  → calls worker_msig.execute(claim_action, claim_sig) → escrow.claim() → InProgress
  → writes result to FastNear KV via RPC (daemon signer — FastNear, not escrow)
  → calls worker_msig.execute(submit_action, submit_sig) → escrow.submit_result() → Verifying
  → escrow creates yield promise
      ↓
LLM Verifier polls list_verifying()
  → reads kv_reference from escrow result
  → fetches full result from FastNear KV (HTTP GET)
  → scores output vs criteria (multi-pass Gemini)
  → calls escrow.resume_verification(data_id, verdict)
  → yield resumes → verification_callback
      ↓
Escrow settles:
  → Passed: ft_transfer to worker's msig + owner fee
  → Failed: ft_transfer agent refund + owner fee
  → Timeout: full refund to agent
  → escrow state: Claimed | Refunded | SettlementFailed
      ↓
Daemon sees settlement on-chain
  → posts kind 41005 (confirmed)
```

---

## What Changes In Each Codebase

### near-escrow changes

**No changes to the escrow contract (src/lib.rs) or msig contract.** They're stable and tested. The contract is already agnostic to how work gets done — it just needs claim → result → verify → settle.

Changes needed in off-chain services:

1. **event_schema.json** — Add kind 41004 (Dispatch) and 41005 (Confirmed) definitions with required/optional tags.

2. **worker.py → replaced by inlayer daemon** — The current worker.py is a stub (asyncio.sleep + JSON). The inlayer daemon becomes the real worker. worker.py becomes deprecated/removed.

3. **relayer.py** — Small change: process both `action`+`action_sig` (create) and `fund_action`+`fund_action_sig` (fund) from a single 41000 event. Calls msig.execute() twice back-to-back: first create_escrow, then ft_transfer_call. The old separate 41003 funding flow still works as fallback.

4. **verifier/** — Small change: `near_client.py` needs a `fetch_kv_result(kv_account, kv_predecessor, kv_key)` function that does a GET to `kv.main.fastnear.com/v0/latest/{kv_account}/{kv_predecessor}/{kv_key}`. The main loop parses the kv_reference JSON from the escrow result, fetches the full data from KV, then scores it.

5. **post_task.py** — Update to sign and post both CreateEscrow + FundEscrow in a single 41000 event. The event tags include `action` + `action_sig` (create) and `fund_action` + `fund_action_sig` (fund). This collapses the old two-event flow into one.

### near-inlayer changes

The daemon is a dumb pipe between the escrow contract and worker agents. It routes tasks, relays worker-signed claim/submit actions via msig.execute(), and handles KV writes. The daemon never does work — external AI agents (each with their own msig) do the work and pre-sign on-chain actions. In escrow mode: worker posts 41002 with signed claim+submit → daemon relays via worker_msig.execute() → KV write → settlement.

1. **Nostr kinds** — Replace 7201-7205 with 41000-41005. Update daemon/nostr.rs:
   - `spawn_nostr_subscriber()` subscribes to kinds 41000, 41001, 41003
   - `handle_nostr_dispatch` (was 7201) → handle kind 41000 (task posted)
   - `handle_nostr_result` (was 7203) → handle kind 41002 (result submitted)
   - `handle_nostr_claim` (was 7204) → handle kind 41001 (claimed)
   - Publish 41004 (dispatched) and 41005 (confirmed) at appropriate points

2. **Daemon main loop (daemon/mod.rs)** — Dual-mode operation:
   - **Escrow mode** (new): Watch Nostr for 41002 events → extract worker msig tags → relay claim via worker_msig.execute() → write result to KV → relay submit_result via worker_msig.execute() → wait for settlement
   - **Direct mode** (existing): Watch inlayer contract for pending requests → execute → resolve on inlayer contract
   - Mode selected by config flag: `execution_mode = "escrow" | "direct" | "both"`

3. **Escrow client** — Module in `daemon/escrow_client.rs`:
   - `poll_until_open(job_id)` — Polls escrow get_escrow() view until status is Open, with timeout (10 min)
   - `claim(job_id)` — Calls escrow claim() with 0.1N stake (daemon signer fallback)
   - `claim_via_msig(worker_msig, claim_action, claim_sig)` — Relays pre-signed claim via worker msig
   - `submit_result(job_id, result)` — Calls escrow submit_result() (daemon signer fallback)
   - `submit_result_via_msig(worker_msig, submit_action, submit_sig)` — Relays pre-signed submit via worker msig
   - `write_kv(key, value)` — Writes result to FastNear KV (daemon signer)
   - `wait_for_settlement(job_id)` — Polls escrow get_escrow() until terminal state
   - `run_escrow_job(...)` — Full lifecycle: claim via worker msig → KV → submit via worker msig → wait

4. **Worker msig relay + KV write** — The daemon's escrow mode path:
   - Worker agent (external AI, has own msig) sees 41004 (FUNDED) on Nostr
   - Worker does the work off-chain, pre-signs claim() and submit_result() with msig key
   - Worker posts kind 41002 to Nostr with signed actions in tags
   - Daemon plumbing thread extracts worker_msig, claim_action, claim_sig, submit_action, submit_sig
   - Daemon relays claim via worker_msig.execute(claim_action, claim_sig) — worker stakes own funds
   - Daemon writes result to FastNear KV via RPC (daemon signer — FastNear, not escrow contract)
   - KV key format: `result/{job_id}` — deterministic, known at worker sign time
   - Daemon relays submit_result via worker_msig.execute(submit_action, submit_sig)
   - KV reference: `{"kv_account": "kv.kampouse.near", "kv_predecessor": "worker_msig", "kv_key": "result/{job_id}"}`
   - Verifier reads full result via HTTP: `GET https://kv.main.fastnear.com/v0/latest/{kv_account}/{kv_predecessor}/{kv_key}`

5. **Config (daemon/manage.rs)** — Add escrow fields to DaemonConfig:
   ```toml
   execution_mode = "escrow"  # or "direct" or "both"
   escrow_contract = "escrow.kampouse.testnet"
   worker_stake_yocto = "100000000000000000000000"  # 0.1 NEAR
   escrow_poll_interval_secs = 10
   escrow_open_timeout_secs = 600
   ```

6. **Identity** — Daemon needs a Nostr key for posting 41001/41002/41004/41005 events. Already has nsec in DaemonConfig for 7200-series. Just update to post with 41xx kinds instead.

### What stays the same

- **Escrow contract** (src/lib.rs) — zero changes
- **Msig contract** (agent-msig/) — zero changes
- **Relayer** (nostr/relayer.py) — small change: process combined create+fund from 41000
- **Verifier** (verifier/) — small change: fetch from KV before scoring
- **Inlayer contract** (contract/) — zero changes (still used for direct mode)
- **WASM executor** (worker/src/executor/) — zero changes
- **Payment verification** (worker/src/daemon/payment.rs) — stays for direct mode

---

## Repo Structure (Post-Merge)

Two repos remain separate. The integration is a config + code bridge in the inlayer daemon.

```
near-escrow/                          near-inlayer/
├── src/lib.rs          # Escrow contract  ├── contract/src/        # Job-queue contract
├── src/tests.rs        # 15 tests         ├── worker/src/
├── agent-msig/         # Msig contract    │   ├── bin/inlayer.rs   # CLI
│   └── src/lib.rs      # + 16 tests       │   ├── daemon/
├── verifier/           # LLM scoring      │   │   ├── mod.rs       # Main loop (dual-mode)
│   ├── main.py                          │   │   ├── escrow_client.rs  # claim, claim_via_msig, submit_result, submit_result_via_msig, write_kv, run_escrow_job
│   ├── scorer.py                        │   │   ├── nostr.rs      # Updated kinds
│   └── near_client.py                   │   │   ├── manage.rs     # Updated config
├── nostr/             # Nostr services    │   │   ├── payment.rs   # Direct mode
│   ├── relayer.py     # Combined create+fund        │   │   └── ...
│   ├── post_task.py   # Combined create+fund         │   ├── executor/        # No change
│   ├── sign_action.py # No change        │   └── ...
│   ├── event_schema.json # Add 41004/05  ├── examples/
│   └── worker.py      # DEPRECATED       └── README.md
├── PLAN.md                               └── SKILL.md
├── MERGED-PLAN.md    # This file
├── WORKER-SPEC.md    # Worker protocol spec (kind 41002) — no daemon dependency
├── WORKER-GUIDE.md   # Worker getting started guide — setup, keys, deployment, flow
└── README.md
```

---

## Implementation Order

### Phase 1: Wire the daemon to escrow (1-2 days) — DONE

1. Add `escrow_client.rs` to inlayer daemon with poll_until_open, claim, claim_via_msig, submit_result, submit_result_via_msig, write_kv, wait_for_settlement, run_escrow_job ✅
2. Update `nostr.rs` to use 41000-41005 kinds ✅
3. Add `execution_mode = "escrow"` config to DaemonConfig ✅
4. Update daemon main loop to support escrow mode ✅
5. Worker msig: WorkerMsigClaim struct, claim_via_msig, submit_result_via_msig ✅
6. Relayer publishes 41004 (FUNDED) after create+fund ✅
7. handle_nostr_result_escrow extracts worker msig tags from 41002 ✅
8. ThreadHealth + supervisor for crash recovery ✅
9. READMEs updated with worker msig flow ✅
10. Python tools deprecated ✅

### Phase 2: End-to-end test on testnet (1-2 days)

1. Deploy escrow contract to testnet
2. Deploy msig contract to testnet
3. Start verifier service (testnet)
4. Start relayer (testnet)
5. Start inlayer daemon in escrow mode (testnet)
6. Post a task via post_task.py
7. Verify full flow: create → fund → claim → execute → verify → settle

### Phase 3: Deploy scripts + docs (1 day)

1. deploy_testnet.sh for escrow + msig
2. deploy_mainnet.sh for escrow + msig
3. Update README with merged architecture
4. Update PLAN.md to reflect merge completion

---

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| Escrow result (8KB) holds KV reference, not full output | Verifier must fetch from KV before scoring | Simple HTTP GET, public endpoint, no auth needed. Result field is ~150 bytes of JSON. |
| Yield timeout (~200 blocks / ~2 min) too short for complex tasks | Verification times out, worker loses | Escrow already handles timeout with full refund to agent. Score threshold is the real gate — timeout just means verifier was slow. |
| "Both" mode runs two watchers | Higher resource usage | Daemon runs Nostr subscriber + contract poller in parallel. Configured per deployment. |
| Daemon needs two NEAR keys (escrow + inlayer) | Config complexity | In escrow mode, daemon only needs one key (for KV writes + relaying worker msig actions). Inlayer contract key only needed in direct mode. Worker has separate msig. |
| Worker stake (0.1N) may be too low for high-value tasks | Sybil risk | Worker stake is anti-spam, not collateral. The verifier is the quality gate. Increase stake per escrow in v2 if needed. |

---

## Open Questions (for Jean)

1. **Verifier reads from KV via HTTP.** The escrow result field holds a KV key (small). Verifier fetches full data from `kv.main.fastnear.com` before scoring. This is reliable — FastNear's HTTP endpoint is public and persistent. No fallback needed. ✅ Resolved.

2. **"Both" mode picks per job from config.** If `execution_mode = "escrow"`, daemon watches Nostr for 41000 events and talks to escrow contract + KV. If `"direct"`, daemon polls inlayer contract and resolves directly. If `"both"`, it runs both watchers in parallel. Different jobs, different paths. ✅ Resolved.

3. **In escrow mode, inlayer contract isn't used.** Escrow holds funds. FastNear KV stores data. Inlayer contract has no role — it only matters in direct mode. ✅ Resolved.

---

## Maybe: Git-over-NEARFS (v2+)

NEARFS is IPFS-compatible storage backed by NEAR. IPFS blocks are stored via `fs_store` contract calls, read via public HTTP gateways (`ipfs.web4.near.page`). Content-addressed, immutable, permanent.

Why it's interesting: a git repo IS a content-addressed filesystem. Git objects map to IPFS blocks naturally. So repos can live on NEARFS — no GitHub, no Nostr relay, no central server.

How it would work:

1. Agent creates a repo with TASK.md, pushes to NEARFS, gets a base CID. Posts 41000 with `repo_cid` tag.
2. Worker clones from `ipfs.web4.near.page/ipfs/{base_cid}`, does the work, pushes updated repo to NEARFS as a new CID.
3. Result in KV: `{repo_base_cid, repo_work_cid, commit_hash}`. Two immutable snapshots — the diff is implicit.
4. Verifier fetches both CIDs via HTTP, diffs them, scores.
5. Agent sees settlement passed, fetches the work repo from NEARFS.

Why NEARFS over alternatives:
- No special tooling needed — public HTTP gateways, standard git clone
- Immutable by default — CIDs can't be tampered with
- Already live on mainnet and testnet
- The CID itself is proof of what was stored

Why "maybe":
- Storing a full git repo on-chain costs gas per block. Unclear how much for a real repo.
- The tooling to push git objects to NEARFS as IPFS blocks doesn't exist yet — would need a git-remote-nearfs transport.
- FastNear KV + plain text results might be good enough for v1. Git repos as work products is a more complex use case that can wait.
