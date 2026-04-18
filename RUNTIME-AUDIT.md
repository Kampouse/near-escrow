# Runtime Risk Audit — NEAR Escrow + Inlayer Daemon
**Date**: 2026-04-15
**Status**: ALL 12 ISSUES FIXED ✓

---

## 1. CRITICAL — Force-Refund Attack on Escrow ✓ FIXED
Added `assert!(env::promise_results_count() > 0, "callback only")` at top of `verification_callback`.

## 2. CRITICAL — Unbounded RATE_LIMITER Growth ✓ FIXED
Added `map.retain()` to prune entries older than 1 hour in `check_rate_limit()`.

## 3. HIGH — Gas Exhaustion Dead-End ✓ FIXED
Added recovery path in `retry_settlement` for Verifying + yield_consumed + no settlement_target.

## 4. HIGH — O(n) resume_verification ✓ FIXED
Added `LookupMap<String, String>` reverse index (data_id → job_id) for O(1) lookup.

## 5. HIGH — Relayer Tight-Loop on Disconnect ✓ FIXED
Changed `continue` to `break` on `RecvTimeoutError::Disconnected`.

## 6. HIGH — Nostr Reconnect Without Backoff ✓ FIXED
Exponential backoff: 1s → 2s → 4s → ... → 60s cap. Resets on success.

## 7. MEDIUM — Score u8 Truncation ✓ FIXED
Added `assert!(score_val <= 255, "score must be 0-255")` before casting.

## 8. MEDIUM — Silent Hex Decode ✓ FIXED
Changed `unwrap_or(0)` to `expect("invalid hex")` — fails loudly on bad input.

## 9. MEDIUM — Supervisor Unbounded Recovery Threads ✓ FIXED
Added `is_recovering: AtomicBool` to `ThreadHealth`. Set on spawn, cleared when new thread pings healthy.

## 10. MEDIUM — Mutex Poisoning Cascade ✓ FIXED
Non-critical mutexes use `.unwrap_or_else(|e| e.into_inner())`. Nonce cache keeps `.unwrap()` (corrupted state = surface immediately).

## 11. LOW — Escrow Records Never Cleaned Up ✓ FIXED
Added `cleanup_completed(max_count: u32) -> u32` — owner-only, removes terminal-state escrows in bounded batches.

## 12. LOW — No Owner Migration ✓ FIXED
Added `transfer_owner(new_owner: AccountId)` — owner-only, emits event.

---

## Test Results

| Suite | Count | Time |
|-------|-------|------|
| Escrow integration | 46/46 | 117s |
| Daemon unit | 142/142 | 1.5s |
| Nostr network | 7/7 | 1.1s |
| **Total** | **195** | **~120s** |
