# Current Session State

## Test Results (Real neard — protocol 152)

### Integration Tests: 73 pass, 0 fail, 2 ignored (156s)
- Full happy path (create → fund → claim → submit → verify → settle) ✅
- Verification failure (agent refund) ✅
- Settlement retry (FT pause/unpause) ✅
- Timeout refunds, double-claim guards, score consistency ✅
- Worker FT withdraw, worker NEAR withdraw ✅
- Yield/resume pipeline fully functional with real neard

### E2E Tests: (not re-run, but same contract logic)

## Root Cause of Previous Failures

The bundled `near-sandbox` binary (v2.0.0, Aug 2024) downloaded by near-workspaces
does NOT properly support `promise_yield` / `promise_yield_resume`. This caused
all yield-dependent tests to fail — escrows stuck at "Verifying" forever.

**Fix**: Use the local neard build (protocol 152) via wrapper script:
```bash
export NEAR_SANDBOX_BIN_PATH=/tmp/near-sandbox-wrapper
cargo test -p integration-tests --test integration
```

The wrapper at `/tmp/near-sandbox-wrapper` is a thin shell script that delegates
to `/Users/asil/.openclaw/workspace/nearcore/target/release/neard`.

## Known Issues
- Test suite is slow (~156s) because each test spins up a fresh neard instance
- near-workspaces 0.16.0 has no `neard_bin()` method — env var workaround needed
- The 2 ignored tests need investigation (not yield-related)

## What's Still Missing for Production
1. `withdraw_balance` for internal wallet workers — implemented but needs audit
2. No standalone verifier service
3. No deploy pipeline (build.sh)
4. No agent cancel after worker claims (timeout-only)
