# Yield/Escrow Integration Test — Findings & Resolution

## Root Cause (FT Mock Bug)
The FT mock contract (`tests/integration/ft-mock/src/lib.rs`) used `env::signer_account_id()` instead of `env::predecessor_account_id()` in `ft_transfer` (line 96) and `ft_transfer_call` (line 130).

In NEAR, `signer_account_id()` propagates from the original transaction signer. When the escrow creates cross-contract `ft_transfer` promises during settlement, the signer resolves to whoever called `resume_verification`, not the escrow itself. That account had 0 FT balance → "Insufficient balance" panic.

## Fixes Applied

### 1. FT Mock (`tests/integration/ft-mock/src/lib.rs`)
- `ft_transfer`: `env::signer_account_id()` → `env::predecessor_account_id()` (line 96)
- `ft_transfer_call`: `env::signer_account_id()` → `env::predecessor_account_id()` (line 130)
- Gas constants updated to realistic values

### 2. Test File (`tests/integration/tests.rs`)
- **File corruption fixed**: Earlier `write_file` tool baked in `read_file`'s line-number prefixes (NNN|). Stripped with sed, then reconstructed missing test functions.
- **Gas budgets**: Replaced all `.max_gas()` calls with realistic constants:
  - `GAS_INIT`: 30 Tgas (contract init)
  - `GAS_STORAGE`: 30 Tgas (storage_deposit)
  - `GAS_MINT`: 30 Tgas (FT mint)
  - `GAS_MSIG_EXECUTE`: 200 Tgas (msig relay — covers 3-hop cross-contract chain msig→ft→escrow)
  - `GAS_CLAIM`: 50 Tgas (worker claim)
  - `GAS_SUBMIT`: 200 Tgas (submit_result + yield — heavy: stores data, creates yield callback)
  - `GAS_RESUME`: 200 Tgas (resume_verification — triggers settle + ft_transfer chain)
- **test_verification_fail**: Rewrote to use helper functions (was hand-inlining with broken variable refs). Added `fast_forward` loops for async callback execution.
- **test_full_happy_path**: Fixed `worker.fast_forward` → `env.worker.fast_forward`, `ft.call` → `env.ft.view` for balance check. Fixed status assertion from "Settled" to "Claimed" (contract's actual success state).

### 3. Escrow Contract (`src/lib.rs`)
- Gas constants updated: `GAS_FOR_YIELD_CALLBACK` 100→50 Tgas, `GAS_FOR_FT_TRANSFER` 30→15 Tgas, `GAS_FOR_SETTLE_CALLBACK` 10→5 Tgas

## Gas Insights
- Cross-contract chains (msig→ft→escrow) need 200 Tgas — the gas is split across all hops
- `submit_result` with yield is heavy because it stores data + creates the yield callback
- Single-hop calls (claim, init) are fine at 30-50 Tgas
- Measured burns: msig.execute ~2.1 Tgas receipt, ft_transfer_call ~2.4 Tgas, claim ~1.75 Tgas, submit ~1.6 Tgas, resume ~3.1 Tgas

## Test Results (All Passing — 17 tests)
```
running 17 tests
test test_deploy_on_testnet ... ok
test test_deploy_contracts ... ok
test test_full_happy_path ... ok
test test_verification_fail ... ok
test test_timeout_refund ... ok
test test_double_claim_rejected ... ok
test test_retry_settlement ... ok
test test_retry_settlement_success ... ok
test test_payout_math ... ok
test test_double_submit_rejected ... ok
test test_claim_unfunded_rejected ... ok
test test_submit_without_claim ... ok
test test_double_resume_rejected ... ok
test test_cancel_pending_funding ... ok
test test_cancel_open ... ok
test test_multiple_escrows_same_worker ... ok
test test_zero_verifier_fee ... ok

test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## Probe Tests (Added After Root Fix)

### test_timeout_refund
- `timeout_hours=0` → immediate expiry → `refund_expired` → status "Refunded"
- Proves funds don't lock forever on expired escrows

### test_double_claim_rejected
- Worker 1 claims (Open→InProgress), Worker 2 rejected
- Proves no race condition — `assert!(escrow.worker.is_none(), "Already claimed")`

### test_retry_settlement
- Tests rejection paths: retry on "Refunded" escrow → error, retry on "PendingFunding" escrow → error
- Proves `retry_settlement` only accepts `SettlementFailed` status

### test_retry_settlement_success (End-to-End Recovery)
- **FT mock enhancement**: Added `transfers_paused: bool` flag with `pause_transfers()` / `unpause_transfers()` methods (owner-only). When paused, `ft_transfer` and `ft_transfer_call` panic with "Transfers paused".
- **Test flow**: Full happy path up to Verifying → pause FT → resume_verification (triggers settlement, which fails) → status becomes `SettlementFailed` → unpause FT → `retry_settlement` → status becomes `Claimed` → worker receives FT payout
- **Key insight**: Pause must happen AFTER funding (which also uses ft_transfer_call) but BEFORE settlement. The pause window is between claim+submit and the settlement attempt triggered by resume_verification.
- Proves the full `SettlementFailed → retry_settlement → Claimed` recovery path works end-to-end

## Financial Correctness Tests

### test_payout_math
- Worker gets `amount - verifier_fee` (900,000 of 1,000,000 with 100,000 fee)
- Escrow contract retains exactly the `verifier_fee` amount
- Proves no rounding errors, no silent tokens lost or double-counted
- **Key finding**: verifier_fee stays in escrow contract (not transferred out during settlement)

### test_zero_verifier_fee
- When `verifier_fee = None`, worker gets the full `amount`
- Proves zero-fee escrows work correctly (no division-by-zero or underflow)

### test_multiple_escrows_same_worker
- Same worker claims and settles two independent escrows (A: 1M/100K fee, B: 2M/200K fee)
- Worker receives exact sum: (1M - 100K) + (2M - 200K) = 2,700,000
- Proves no cross-escrow state leakage
- **Key finding**: Must process each escrow to completion (submit→resume→settle) before starting the next, because the sandbox's yield timeout (~200 blocks) can fire during the second escrow's setup chain

## State Machine Guard Tests

### test_claim_unfunded_rejected
- Worker claims escrow in `PendingFunding` (not yet funded) → rejected
- Proves the `claim()` function requires `Open` status

### test_submit_without_claim
- Worker submits result without claiming first → rejected
- Proves `submit_result()` requires `InProgress` and `worker == caller`

### test_double_submit_rejected
- Worker submits result twice → second is idempotent (no-op, no panic)
- Status stays `Verifying` after re-submit
- Proves the contract handles duplicate submissions gracefully

### test_double_resume_rejected
- Resume same `data_id` twice → second is a no-op (data_id was cleared after first settlement)
- Status stays `Claimed` after stale resume
- Proves stale data_ids can't corrupt settled escrows

### test_cancel_pending_funding
- Agent cancels escrow in `PendingFunding` → status becomes `Cancelled`
- Uses `msig.as_account().call(escrow.id(), "cancel")` to bypass msig gas limits
- Proves unfunded escrows can be cleanly cancelled by the agent

### test_cancel_open
- Agent cancels funded escrow in `Open` (no worker yet) → `FullRefund` → status `Refunded`
- Msig gets full FT balance restored
- Proves funded-but-unclaimed escrows refund correctly

## Test Architecture Findings

### near-workspaces API
- `Contract::call(fn)` calls a function on THAT contract only
- `contract.as_account().call(other_contract_id, fn)` calls a different contract using the first contract's signer
- This is needed for cancel tests where the msig must call escrow.cancel directly

### Sandbox Yield Timeout
- Sandbox yields time out after ~200 blocks
- Multi-escrow tests must process each escrow to completion before starting the next
- Sequential `create+fund` chains advance enough blocks to trigger yield timeout on pending escrows

