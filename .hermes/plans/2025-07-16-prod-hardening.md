# Production Hardening Plan — near-escrow

**Goal:** Fix all 11 issues identified in the production readiness audit.

**Architecture:** All changes in `src/lib.rs` (contract) + `tests/integration/tests.rs` (tests). Each task is a single focused patch with verification.

**Build/Test commands:**
- Build WASM: `cargo build --release --target wasm32-unknown-unknown`
- Run tests: `NEAR_SANDBOX_BIN=/Users/asil/.openclaw/workspace/near-escrow/target/debug/build/near-sandbox-075fa242981621a0/out/.near/near-sandbox-2.11.0/near-sandbox cargo test -p integration-tests`

---

## Task 1: Add reverse index for data_id → job_id (fix #1)

**Problem:** `resume_verification` (lines 554-561) iterates ALL escrows to find a data_id match. O(n) gas cost.

**Fix:** Add `data_id_index: UnorderedMap<String, String>` to contract struct. Populate on yield create, read on resume, clean up on callback/settlement.

**Changes in `src/lib.rs`:**

1a. Add field to `EscrowContract` (line ~204):
```rust
pub struct EscrowContract {
    owner: AccountId,
    escrows: UnorderedMap<String, Escrow>,
    verifier: AccountId,
    data_id_index: UnorderedMap<String, String>,  // hex(data_id) → job_id
}
```

1b. Update `new()` (line ~223): add `data_id_index: UnorderedMap::new(b"d"),`

1c. After yield creation in `submit_result` standard branch (after line 518): add index entry:
```rust
self.data_id_index.insert(&hex_encode(data_id.as_ref()), &job_id);
```

1d. After yield creation in `designate_winner` (after line 943): add index entry:
```rust
self.data_id_index.insert(&hex_encode(data_id.as_ref()), &job_id);
```

1e. Replace O(n) scan in `resume_verification` (lines 552-561) with:
```rust
let matching_job = self.data_id_index.get(&data_id_hex);
if let Some(jid) = matching_job {
    let escrow = self.escrows.get(&jid).expect("escrow vanished during index lookup");
    assert!(!escrow.yield_consumed, "Yield already consumed");
    let mut escrow = escrow;
    escrow.yield_consumed = true;
    self.escrows.insert(&jid, &escrow);
}
```

1f. Clean up index in `verification_callback` (after line 707 where data_id is set to None): remove stale index entry:
```rust
if let Some(ref did) = old_data_id {  // need to capture before clearing
    self.data_id_index.remove(&hex_encode(did.as_ref()));
}
```
Actually simpler: clear the index when data_id is cleared:
```rust
// Before clearing data_id, remove from index
if let Some(ref did) = escrow.data_id {
    self.data_id_index.remove(&hex_encode(did.as_ref()));
}
escrow.data_id = None;
```

**Verification:** `cargo build --release --target wasm32-unknown-unknown` compiles, all existing tests pass.

---

## Task 2: Fix list_verifying access control (fix #2)

**Problem:** Comment says "verifier-only" but no actual check. near-sdk 5.x blocks `predecessor_account_id()` in views.

**Fix:** Remove misleading comment. Document that this is a public view — data_id exposure is intentional (verifier needs it, anyone can see it). The result content is already redacted.

**Changes in `src/lib.rs`:**

2a. Replace comment on `list_verifying` (lines 1182-1184) with:
```rust
/// List escrows in Verifying state with their data_id.
/// PUBLIC view — data_id is needed by the verifier to call resume_verification.
/// Result content is redacted — only task metadata is returned.
/// If access control is needed, use an indexing service or offchain auth.
```

**Verification:** Build + tests pass.

---

## Task 3: Replace `let _ =` with logged failures (fix #8)

**Problem:** Silent failures on Promise transfers — stake could be lost.

**Fix:** Add `log!` warnings for each transfer that could fail. We can't `.as_return()` on these (they're fire-and-forget NEAR transfers, not FT calls), but we should at minimum log.

**Changes in `src/lib.rs`:**

3a. Line 666 (malformed verdict, worker stake refund):
```rust
// Before:
let _ = Promise::new(worker.clone())
    .transfer(NearToken::from_yoctonear(stake.0));
// After:
Promise::new(worker.clone())
    .transfer(NearToken::from_yoctonear(stake.0));
```
Actually NEAR `Promise::transfer` never fails (just drains contract balance). So `let _ =` is fine for NEAR transfers. But let's add logging for safety:
```rust
let stake_u128 = stake.0;
if let Some(ref worker) = escrow.worker {
    Promise::new(worker.clone())
        .transfer(NearToken::from_yoctonear(stake_u128));
    log!("Refunding worker stake: {} yoctoNEAR to {}", stake_u128, worker);
}
```

3b. Apply same pattern to:
- Line 689 (timeout, worker stake refund)
- Line 818 (storage deposit refund to agent)  
- Line 824 (settle_callback, worker stake refund)
- Line 1018 (cancel PendingFunding, storage refund)
- Line 1051 (refund_expired PendingFunding, storage refund)
- Line 1065 (refund_expired InProgress, worker stake to agent)

**Verification:** Build + tests pass. Check logs appear in test output.

---

## Task 4: Add deadline_block check to designate_winner (fix #11)

**Problem:** Agent can designate winner after deadline_block has passed.

**Fix:** Add deadline check in designate_winner after the Open assertion.

**Changes in `src/lib.rs`:**

After line 904 (`assert_eq!(escrow.status, EscrowStatus::Open, "Must be Open");`), add:
```rust
// Deadline check — agent must designate before submission deadline
if let Some(deadline) = escrow.deadline_block {
    assert!(
        env::block_height() <= deadline,
        "Cannot designate winner after deadline (block {} > {})",
        env::block_height(),
        deadline
    );
}
```

**Verification:** Build + tests pass. Add a test: create competitive escrow with deadline_block=1, advance blocks past deadline, call designate_winner → expect panic.

---

## Task 5: Add migrate() function (fix #6)

**Problem:** No migration path. `new()` asserts `!env::state_exists()`.

**Fix:** Add owner-only `migrate()` that can update state.

**Changes in `src/lib.rs`:**

After `new()` (line 228), add:
```rust
/// Owner-only migration. Called when upgrading contract code.
/// State must already exist — panics on fresh deployments.
pub fn migrate() {
    assert!(
        env::state_exists(),
        "No state to migrate — use new() for fresh deploy"
    );
    assert_eq!(
        env::predecessor_account_id(),
        EscrowContract::owner_from_state(),
        "Only owner can migrate"
    );
    // Add migration logic here as needed for future versions.
    // Currently a no-op — state format is unchanged.
    log!("Migration complete — no state changes needed");
}
```

Wait — `owner_from_state()` doesn't exist. Simpler approach:

```rust
#[private]
pub fn migrate() {
    // Only contract itself (via owner cross-contract call or redeploy) can call this.
    // For now, no-op — state format unchanged.
    // Add field additions/removals here in future versions.
    log!("Migration complete");
}
```

Actually, `#[private]` is enough — only the contract account can call it, which means only the owner (who controls the contract account) can trigger it via code deploy.

**Verification:** Build + tests pass.

---

## Task 6: Add competitive worker stake on submit_result (fix #4)

**Problem:** Competitive mode workers submit for free — spam potential.

**Fix:** Require WORKER_STAKE_YOCTO attached deposit on competitive submit_result. Store in Submission. Refund non-winners on designate_winner. Refund winner on settlement.

**Changes in `src/lib.rs`:**

6a. Add `stake` field to `Submission` struct (line 79-82):
```rust
pub struct Submission {
    pub worker: AccountId,
    pub result: String,
    pub stake: U128,  // Anti-spam bond, refunded to non-winners on designate
}
```

6b. Make `submit_result` `#[payable]` for competitive branch. Change method signature (line 434) — already `&mut self`. Add:
```rust
// At top of submit_result, after caller/escrow setup:
// For competitive mode, require stake
if escrow.mode == EscrowMode::Competitive {
    let attached = env::attached_deposit().as_yoctonear();
    assert!(
        attached >= WORKER_STAKE_YOCTO,
        "Competitive submission requires {} yoctoNEAR stake",
        WORKER_STAKE_YOCTO
    );
}
```

6c. Update Submission creation (line 468-471):
```rust
escrow.submissions.push(Submission {
    worker: caller.clone(),
    result: result.clone(),
    stake: U128(attached),
});
```
Need to capture `attached` before the match on mode. Refactor: compute `attached` at top of function for competitive mode.

6d. In `designate_winner`, after choosing winner (after line 921), refund all non-winners:
```rust
// Refund stakes of non-winning submissions
for (i, sub) in escrow.submissions.iter().enumerate() {
    if i != idx {
        Promise::new(sub.worker.clone())
            .transfer(NearToken::from_yoctonear(sub.stake.0));
    }
}
// Winner's stake goes into escrow.worker_stake for normal settlement flow
escrow.worker_stake = Some(escrow.submissions[idx].stake);
```

6e. In `EscrowView::From<Escrow>`, no change needed — Submission isn't in view.

6f. Update `agent-msig/src/lib.rs` if it forwards submit_result — it needs to attach stake too.

**Verification:** Build + tests. Update competitive tests to attach stake. Non-competitive tests unchanged.

---

## Task 7: Cap result size for competitive submissions (fix #5)

**Problem:** Each competitive submission stores up to 8KB. 100 submissions = 800KB gas bomb.

**Fix:** Add `MAX_COMPETITIVE_RESULT_LEN = 2048` constant. Assert in competitive branch.

**Changes in `src/lib.rs`:**

7a. Add constant (after line 214):
```rust
const MAX_COMPETITIVE_RESULT_LEN: usize = 2048;
```

7b. In `submit_result` competitive branch (after line 437, before the match), add competitive-specific cap:
```rust
if escrow.mode == EscrowMode::Competitive {
    assert!(
        result.len() <= MAX_COMPETITIVE_RESULT_LEN,
        "Competitive result too long (max {} bytes)",
        MAX_COMPETITIVE_RESULT_LEN
    );
}
```

**Verification:** Build + tests pass.

---

## Task 8: Relax cleanup_completed to anyone (fix #7)

**Problem:** Only owner can cleanup. If owner disappears, terminal escrows are stuck in state.

**Fix:** Allow anyone to clean up terminal escrows. Owner-only was unnecessary — cleaning terminal state is always safe.

**Changes in `src/lib.rs`:**

8a. Replace lines 965-969:
```rust
// Before:
assert_eq!(
    env::predecessor_account_id(),
    self.owner,
    "Only owner can cleanup"
);
// After:
// Anyone can cleanup — removing terminal state is always safe and frees storage
```

8b. Update doc comment (line 962): Remove "Only callable by owner." → "Callable by anyone."

**Verification:** Build + tests pass. Update test that checks owner-only.

---

## Task 9: Make constants configurable by owner (fix #10)

**Problem:** STORAGE_DEPOSIT_YOCTO, WORKER_STAKE_YOCTO are hardcoded.

**Fix:** Store them in contract state, set in `new()`, updateable by owner.

**Changes in `src/lib.rs`:**

9a. Add fields to `EscrowContract`:
```rust
pub struct EscrowContract {
    owner: AccountId,
    escrows: UnorderedMap<String, Escrow>,
    verifier: AccountId,
    data_id_index: UnorderedMap<String, String>,
    storage_deposit_yocto: u128,
    worker_stake_yocto: u128,
}
```

9b. Update `new()`:
```rust
Self {
    owner: owner.clone(),
    escrows: UnorderedMap::new(b"e"),
    verifier: verifier_account_id.unwrap_or(owner),
    data_id_index: UnorderedMap::new(b"d"),
    storage_deposit_yocto: STORAGE_DEPOSIT_YOCTO,
    worker_stake_yocto: WORKER_STAKE_YOCTO,
}
```

9c. Add owner setters:
```rust
pub fn set_storage_deposit(&mut self, amount: U128) {
    assert_eq!(env::predecessor_account_id(), self.owner, "Only owner");
    self.storage_deposit_yocto = amount.0;
}

pub fn set_worker_stake(&mut self, amount: U128) {
    assert_eq!(env::predecessor_account_id(), self.owner, "Only owner");
    self.worker_stake_yocto = amount.0;
}
```

9d. Replace all `STORAGE_DEPOSIT_YOCTO` usages with `self.storage_deposit_yocto`.
9e. Replace all `WORKER_STAKE_YOCTO` usages with `self.worker_stake_yocto`.

**Verification:** Build + tests pass.

---

## Task 10: Document O(n) view tradeoff (fix #3)

**Problem:** Views iterate UnorderedMap. Not a blocker under 10K escrows.

**Decision:** Accept O(n) with documented ceiling. Secondary indexes add complexity and storage cost that isn't justified yet. Pagination caps (max 100) bound the gas per call.

**Changes in `src/lib.rs`:**

10a. Add doc comments to each view method noting the O(n) tradeoff:
```rust
/// NOTE: O(n) scan over all escrows. Safe for <10K escrows with pagination caps.
/// For higher scale, use an offchain indexer.
```

Add to: `list_open`, `list_by_agent`, `list_by_worker`, `list_by_status`, `get_stats`.

**Verification:** Build + tests pass.

---

## Task 11: Bump event version (fix #9)

**Problem:** Using custom standard "escrow" v3.0.0.

**Fix:** Bump to v3.1.0 after these changes. Document what changed.

**Changes in `src/lib.rs`:**

11a. Update `emit_event` (line 174):
```rust
"version": "3.1.0",
```

**Verification:** Build + tests pass.

---

## Execution Order

1. Task 1 (data_id_index) — structural change, do first
2. Task 9 (configurable constants) — structural change, do second (depends on Task 1 struct)
3. Task 5 (migrate) — add after new()
4. Task 6 (competitive stake) — needs Task 9's configurable constants
5. Task 7 (competitive result cap) — quick
6. Task 4 (deadline_block check) — quick
7. Task 8 (cleanup anyone) — quick
8. Task 3 (logged transfers) — quick
9. Task 2 (list_verifying comment) — quick
10. Task 10 (view docs) — quick
11. Task 11 (version bump) — last

Build WASM after all tasks. Run full test suite. Fix any regressions.

---

## Tests to Add

- `test_resume_verification_uses_index` — verify resume_verification doesn't iterate all escrows (gas comparison test)
- `test_competitive_submit_requires_stake` — competitive submit without stake → panic
- `test_competitive_non_winner_refund` — designate_winner refunds non-winners
- `test_designate_winner_after_deadline` — designate after deadline → panic
- `test_cleanup_anyone` — non-owner can cleanup terminal escrows
- `test_migrate` — migrate on existing state succeeds, on fresh state panics
- `test_set_storage_deposit_owner_only` — non-owner rejected
