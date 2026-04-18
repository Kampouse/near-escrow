# Current Session State

## What's Running
- **PID 57144** — nearcore release build with ACTION-FAILED + CONTRACT-LOG instrumentation
  - Command: `cargo build -p neard --release --features "sandbox,nightly,json_rpc"`
  - Working dir: `/Users/asil/.openclaw/workspace/nearcore`
  - Session: `proc_21e74e420f2d`
  - Started ~7 min ago, expected total ~11 min

## What To Do When Build Finishes
1. Swap binary:
   ```bash
   cp /Users/asil/.openclaw/workspace/nearcore/target/release/neard \
      /Users/asil/.openclaw/workspace/near-escrow/target/debug/build/near-sandbox-075fa242981621a0/out/.near/near-sandbox-2.11.0/near-sandbox
   ```
2. Clear debug log: `> /tmp/nearcore-yield-debug.log`
3. Run test:
   ```bash
   cd /Users/asil/.openclaw/workspace/near-escrow
   cargo test --manifest-path tests/integration/Cargo.toml -- test_full_happy_path --exact --nocapture 2>&1 | tail -60
   ```
4. Read debug log:
   ```bash
   grep -E 'ACTION-FAILED|CONTRACT-LOG' /tmp/nearcore-yield-debug.log
   ```
5. The ACTION-FAILED line will show the exact WASM panic message
6. The CONTRACT-LOG lines will show the contract's env::log_str output

## Current Hypothesis
Nearcore yield/resume pipeline is CORRECT. The problem is inside the WASM:
- verification_callback fires at runtime level but panics
- State reverts (verdict stays null, status stays "Verifying")
- Most likely panic in `_settle_escrow()` or in the callback's data parsing

## Key Evidence
- Debug log shows: YIELD-STORE ✅ → YIELD-RESUME FOUND ✅ → APPLY-RECEIPT verification_callback ✅
- But: NO settle_callback, NO ft_transfer from settlement, verdict=null
- Every APPLY-RECEIPT appears twice (logging artifact, not double execution)

## Files Modified (uncommitted)
- `/Users/asil/.openclaw/workspace/nearcore/runtime/runtime/src/lib.rs` — instrumentation patches
- `/Users/asil/.openclaw/workspace/near-escrow/YIELD-BUG-FINDINGS.md` — findings doc
