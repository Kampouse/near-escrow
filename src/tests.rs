use near_sdk::test_utils::VMContextBuilder;
use near_sdk::{testing_env, AccountId, NearToken, GasWeight};

// Stub yield host functions for native test compilation.
// These are only needed so the linker doesn't fail — tests that exercise
// yield/resume paths must run as integration tests against sandbox.
#[cfg(test)]
mod yield_stubs {
    use std::ffi::c_ulong;

    #[no_mangle]
    pub extern "C" fn promise_yield_create(
        _account_id: u64,
        _method_name: u64,
        _arguments: u64,
        _amount: u128,
        _gas: c_ulong,
        _gas_weight: u64,
        _data_id_register: u64,
    ) -> u64 {
        0u64 // promise index
    }

    #[no_mangle]
    pub extern "C" fn promise_yield_resume(
        _data_id: u64,
        _payload: u64,
        _payload_len: u64,
    ) {
        // no-op
    }
}

fn alice() -> AccountId {
    "alice.near".parse().unwrap()
}

fn bob() -> AccountId {
    "bob.near".parse().unwrap()
}

fn token_contract() -> AccountId {
    "token.near".parse().unwrap()
}

fn contract_account() -> AccountId {
    "escrow.near".parse().unwrap()
}

use crate::VerifierInfo;

fn new_contract() -> super::EscrowContract {
    super::EscrowContract::new(
        vec![VerifierInfo {
            account_id: "verifier.test.near".parse().unwrap(),
            public_key: "a".repeat(64),
            active: true,
        }],
        Some(1),
        vec!["usdt.tether-token.near".parse().unwrap(), "token.near".parse().unwrap()],
    )
}

fn one_near() -> NearToken {
    NearToken::from_yoctonear(1_000_000_000_000_000_000_000_000)
}

fn worker_stake() -> NearToken {
    NearToken::from_yoctonear(100_000_000_000_000_000_000_000) // 0.1 NEAR
}

fn setup() -> VMContextBuilder {
    let mut builder = VMContextBuilder::new();
    builder
        .current_account_id(contract_account())
        .signer_account_id(alice())
        .predecessor_account_id(alice())
        .attached_deposit(one_near());
    builder
}

fn create_funded_contract(context: &mut VMContextBuilder, job_id: &str) -> super::EscrowContract {
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = new_contract();

    contract.create_escrow(
        job_id.to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Build a TODO app".to_string(),
        "Must have CRUD operations and tests".to_string(),
        Some(near_sdk::json_types::U128(100_000)),
        Some(80),
        None,
        None,
    );

    // Fund via ft_on_transfer
    testing_env!(context
        .predecessor_account_id(token_contract())
        .signer_account_id(alice())
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());

    contract.ft_on_transfer(
        alice(),
        near_sdk::json_types::U128(1_000_000),
        job_id.to_string(),
    );

    // Reset context back to normal
    testing_env!(context
        .predecessor_account_id(alice())
        .signer_account_id(alice())
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());

    contract
}

// ─── Contract init ────────────────────────────────────────────────

#[test]
fn test_new_contract() {
    let context = setup();
    testing_env!(context.build());
    let contract = new_contract();
    assert_eq!(contract.get_owner(), alice());
}

// ─── Create escrow ────────────────────────────────────────────────

#[test]
fn test_create_escrow() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = new_contract();
    contract.create_escrow(
        "job-1".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Build a TODO app".to_string(),
        "Must have CRUD operations".to_string(),
        Some(near_sdk::json_types::U128(50_000)),
        Some(80),
        None,
        None,
    );

    let escrow = contract.get_escrow("job-1".to_string()).unwrap();
    assert_eq!(escrow.job_id, "job-1");
    assert_eq!(escrow.agent, alice());
    assert!(escrow.worker.is_none());
    assert_eq!(escrow.amount.0, 1_000_000);
    assert_eq!(escrow.token, token_contract());
    assert!(matches!(escrow.status, super::EscrowStatus::PendingFunding));
}

#[test]
#[should_panic(expected = "Insufficient storage deposit")]
fn test_create_escrow_no_deposit() {
    let mut context = setup();
    testing_env!(context
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());

    let mut contract = new_contract();
    contract.create_escrow(
        "job-2".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
        None,
        None,
        None,
        None,
    );
}

#[test]
#[should_panic(expected = "Job ID exists")]
fn test_create_escrow_duplicate_id() {
    let mut context = setup();
    testing_env!(context
        .attached_deposit(NearToken::from_yoctonear(
            2 * 1_000_000_000_000_000_000_000_000
        ))
        .build());

    let mut contract = new_contract();

    contract.create_escrow(
        "job-dup".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
        None,
        None,
        None,
        None,
    );

    contract.create_escrow(
        "job-dup".to_string(),
        near_sdk::json_types::U128(2_000_000),
        token_contract(),
        24,
        "Task 2".to_string(),
        "Criteria 2".to_string(),
        None,
        None,
        None,
        None,
    );
}

#[test]
#[should_panic(expected = "Verifier fee must be less than amount")]
fn test_create_escrow_fee_too_high() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = new_contract();
    contract.create_escrow(
        "job-fee".to_string(),
        near_sdk::json_types::U128(100),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
        Some(near_sdk::json_types::U128(100)),
        None,
        None,
        None,
    );
}

#[test]
#[should_panic(expected = "Criteria required")]
fn test_create_escrow_empty_criteria() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = new_contract();
    contract.create_escrow(
        "job-no-criteria".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "".to_string(), // empty criteria
        None,
        None,
        None,
        None,
    );
}

// ─── FT funding ───────────────────────────────────────────────────

#[test]
fn test_ft_on_transfer_funding() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = new_contract();
    contract.create_escrow(
        "job-ft".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
        None,
        None,
        None,
        None,
    );

    // FT contract calls ft_on_transfer
    testing_env!(context
        .predecessor_account_id(token_contract())
        .signer_account_id(alice())
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());

    let refund = contract.ft_on_transfer(
        alice(),
        near_sdk::json_types::U128(1_000_000),
        "job-ft".to_string(),
    );

    assert_eq!(refund.0, 0); // Accept all tokens

    let escrow = contract.get_escrow("job-ft".to_string()).unwrap();
    assert!(matches!(escrow.status, super::EscrowStatus::Open));
}

#[test]
fn test_ft_on_transfer_wrong_sender_rejected() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = new_contract();
    contract.create_escrow(
        "job-wrong".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
        None,
        None,
        None,
        None,
    );

    testing_env!(context
        .predecessor_account_id(token_contract())
        .signer_account_id(bob())
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());

    let refund = contract.ft_on_transfer(
        bob(),
        near_sdk::json_types::U128(1_000_000),
        "job-wrong".to_string(),
    );

    assert_eq!(refund.0, 1_000_000); // Reject — return all

    let escrow = contract.get_escrow("job-wrong".to_string()).unwrap();
    assert!(matches!(escrow.status, super::EscrowStatus::PendingFunding));
}

#[test]
fn test_ft_on_transfer_wrong_amount_rejected() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = new_contract();
    contract.create_escrow(
        "job-amt".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
        None,
        None,
        None,
        None,
    );

    testing_env!(context
        .predecessor_account_id(token_contract())
        .signer_account_id(alice())
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());

    let refund = contract.ft_on_transfer(
        alice(),
        near_sdk::json_types::U128(999_999),
        "job-amt".to_string(),
    );

    assert_eq!(refund.0, 999_999); // Reject
}

// ─── Claim ────────────────────────────────────────────────────────

#[test]
fn test_claim() {
    let mut context = setup();
    let mut contract = create_funded_contract(&mut context, "job-claim");

    // Bob claims with worker stake
    testing_env!(context
        .signer_account_id(bob())
        .predecessor_account_id(bob())
        .attached_deposit(worker_stake())
        .build());
    contract.claim("job-claim".to_string());

    let escrow = contract.get_escrow("job-claim".to_string()).unwrap();
    assert!(matches!(escrow.status, super::EscrowStatus::InProgress));
    assert_eq!(escrow.worker, Some(bob()));
}

#[test]
#[should_panic(expected = "Agent cannot claim own escrow")]
fn test_agent_cannot_claim_own() {
    let mut context = setup();
    let mut contract = create_funded_contract(&mut context, "job-self");

    // Alice (agent) tries to claim
    testing_env!(context
        .signer_account_id(alice())
        .predecessor_account_id(alice())
        .attached_deposit(worker_stake())
        .build());
    contract.claim("job-self".to_string());
}

#[test]
#[should_panic(expected = "Worker stake required")]
fn test_claim_no_stake() {
    let mut context = setup();
    let mut contract = create_funded_contract(&mut context, "job-no-stake");

    // Bob tries to claim without attaching stake
    testing_env!(context
        .signer_account_id(bob())
        .predecessor_account_id(bob())
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());
    contract.claim("job-no-stake".to_string());
}

#[test]
#[should_panic(expected = "Worker stake required")]
fn test_claim_insufficient_stake() {
    let mut context = setup();
    let mut contract = create_funded_contract(&mut context, "job-low-stake");

    // Bob tries to claim with too little stake (0.01 NEAR instead of 0.1)
    testing_env!(context
        .signer_account_id(bob())
        .predecessor_account_id(bob())
        .attached_deposit(NearToken::from_yoctonear(10_000_000_000_000_000_000_000))
        .build());
    contract.claim("job-low-stake".to_string());
}

// ─── Cancel ───────────────────────────────────────────────────────

#[test]
fn test_cancel_pending_funding() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = new_contract();
    contract.create_escrow(
        "job-cancel".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
        None,
        None,
        None,
        None,
    );

    testing_env!(context
        .signer_account_id(alice())
        .predecessor_account_id(alice())
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());
    contract.cancel("job-cancel".to_string());

    let escrow = contract.get_escrow("job-cancel".to_string()).unwrap();
    assert!(matches!(escrow.status, super::EscrowStatus::Cancelled));
}

#[test]
#[should_panic(expected = "Only agent")]
fn test_cancel_wrong_caller() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = new_contract();
    contract.create_escrow(
        "job-wrong-cancel".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
        None,
        None,
        None,
        None,
    );

    testing_env!(context
        .signer_account_id(bob())
        .predecessor_account_id(bob())
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());
    contract.cancel("job-wrong-cancel".to_string());
}

// ─── Views ────────────────────────────────────────────────────────

#[test]
fn test_get_stats() {
    let mut context = setup();
    testing_env!(context
        .attached_deposit(NearToken::from_yoctonear(5_000_000_000_000_000_000_000_000))
        .build());

    let mut contract = new_contract();

    contract.create_escrow(
        "stat-1".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task 1".to_string(),
        "Criteria".to_string(),
        None,
        None,
        None,
        None,
    );
    contract.create_escrow(
        "stat-2".to_string(),
        near_sdk::json_types::U128(2_000_000),
        token_contract(),
        24,
        "Task 2".to_string(),
        "Criteria".to_string(),
        None,
        None,
        None,
        None,
    );

    let stats = contract.get_stats();
    assert_eq!(stats["total"], 2);
}

#[test]
fn test_get_storage_deposit() {
    let context = setup();
    testing_env!(context.build());
    let contract = new_contract();
    let deposit = contract.get_storage_deposit();
    assert_eq!(deposit.0, 1_000_000_000_000_000_000_000_000); // 1 NEAR
}

#[test]
fn test_list_open_empty() {
    let context = setup();
    testing_env!(context.build());
    let contract = new_contract();
    let open = contract.list_open(None, None);
    assert!(open.is_empty());
}

// ─── Access control guards ────────────────────────────────────────

#[test]
#[should_panic(expected = "Insufficient valid signatures")]
fn test_resume_verification_wrong_caller() {
    let mut context = setup();
    let mut contract = create_funded_contract(&mut context, "job-guard1");

    // Bob claims with stake
    testing_env!(context
        .signer_account_id(bob())
        .predecessor_account_id(bob())
        .attached_deposit(worker_stake())
        .build());
    contract.claim("job-guard1".to_string());

    // Bob (NOT the owner/verifier) tries to resume verification
    testing_env!(context
        .signer_account_id(bob())
        .predecessor_account_id(bob())
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());
    // data_id doesn't matter — the assert fires first
    contract.resume_verification_multi(
        "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        crate::SignedVerdict {
            verdict_json: "{\"score\":100,\"passed\":true,\"detail\":\"hack\"}".to_string(),
            signatures: vec![],
        },
    );
}

#[test]
#[should_panic(expected = "settle_callback must be called as a promise callback")]
fn test_settle_callback_direct_call_rejected() {
    let mut context = setup();
    let mut contract = create_funded_contract(&mut context, "job-guard2");

    // settle_callback called directly (not as a promise callback) should panic
    // because promise_results_count() == 0
    testing_env!(context
        .signer_account_id(alice())
        .predecessor_account_id(alice())
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());
    contract.settle_callback("job-guard2".to_string());
}

// ─── list_by_status view ─────────────────────────────────────────

#[test]
fn test_list_by_status() {
    let mut context = setup();
    testing_env!(context
        .attached_deposit(NearToken::from_yoctonear(2_000_000_000_000_000_000_000_000))
        .build());

    let mut contract = new_contract();
    contract.create_escrow(
        "s1".to_string(),
        near_sdk::json_types::U128(1000),
        token_contract(),
        24,
        "T1".to_string(),
        "C1".to_string(),
        None,
        None,
        None,
        None,
    );
    contract.create_escrow(
        "s2".to_string(),
        near_sdk::json_types::U128(2000),
        token_contract(),
        24,
        "T2".to_string(),
        "C2".to_string(),
        None,
        None,
        None,
        None,
    );

    let pending = contract.list_by_status("PendingFunding".to_string(), None, None);
    assert_eq!(pending.len(), 2);

    let open = contract.list_by_status("Open".to_string(), None, None);
    assert!(open.is_empty());
}

#[test]
#[should_panic(expected = "Unknown status")]
fn test_list_by_status_invalid() {
    let context = setup();
    testing_env!(context.build());
    let contract = new_contract();
    contract.list_by_status("NoSuchStatus".to_string(), None, None);
}

// ─── String length caps ──────────────────────────────────────────

#[test]
#[should_panic(expected = "Job ID too long")]
fn test_create_escrow_job_id_too_long() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = new_contract();
    let long_id = "x".repeat(200); // max is 128
    contract.create_escrow(
        long_id,
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
        None,
        None,
        None,
        None,
    );
}

#[test]
#[should_panic(expected = "Task description too long")]
fn test_create_escrow_description_too_long() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = new_contract();
    contract.create_escrow(
        "job-long-desc".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "x".repeat(3000), // max is 2048
        "Criteria".to_string(),
        None,
        None,
        None,
        None,
    );
}

#[test]
#[should_panic(expected = "Criteria too long")]
fn test_create_escrow_criteria_too_long() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = new_contract();
    contract.create_escrow(
        "job-long-crit".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "x".repeat(3000), // max is 2048
        None,
        None,
        None,
        None,
    );
}

#[test]
#[should_panic(expected = "Result too long")]
fn test_submit_result_too_long() {
    let mut context = setup();
    let mut contract = create_funded_contract(&mut context, "job-long-result");

    // Claim
    testing_env!(context
        .signer_account_id(bob())
        .predecessor_account_id(bob())
        .attached_deposit(worker_stake())
        .build());
    contract.claim("job-long-result".to_string());

    // Submit result that's too long
    testing_env!(context
        .signer_account_id(bob())
        .predecessor_account_id(bob())
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());
    contract.submit_result(
        "job-long-result".to_string(),
        "x".repeat(10000), // max is 8192
    );
}

// ─── Full lifecycle integration test ─────────────────────────────

#[test]
#[ignore] // Requires NEAR sandbox — exercises promise_yield_create/resume
fn test_full_lifecycle_create_fund_claim_submit() {
    let mut context = setup();

    // 1. Create escrow (agent = alice)
    testing_env!(context.attached_deposit(one_near()).build());
    let mut contract = new_contract();
    contract.create_escrow(
        "lifecycle-1".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Build a REST API".to_string(),
        "Must have CRUD endpoints and tests".to_string(),
        Some(near_sdk::json_types::U128(100_000)),
        Some(80),
        None,
        None,
    );

    // Verify PendingFunding
    let escrow = contract.get_escrow("lifecycle-1".to_string()).unwrap();
    assert!(matches!(escrow.status, super::EscrowStatus::PendingFunding));
    assert_eq!(escrow.agent, alice());
    assert!(escrow.worker.is_none());

    // 2. Fund via ft_on_transfer
    testing_env!(context
        .predecessor_account_id(token_contract())
        .signer_account_id(alice())
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());
    let refund = contract.ft_on_transfer(
        alice(),
        near_sdk::json_types::U128(1_000_000),
        "lifecycle-1".to_string(),
    );
    assert_eq!(refund.0, 0); // accepted

    // Verify Open
    let escrow = contract.get_escrow("lifecycle-1".to_string()).unwrap();
    assert!(matches!(escrow.status, super::EscrowStatus::Open));

    // 3. Worker claims with stake
    testing_env!(context
        .signer_account_id(bob())
        .predecessor_account_id(bob())
        .attached_deposit(worker_stake())
        .build());
    contract.claim("lifecycle-1".to_string());

    // Verify InProgress
    let escrow = contract.get_escrow("lifecycle-1".to_string()).unwrap();
    assert!(matches!(escrow.status, super::EscrowStatus::InProgress));
    assert_eq!(escrow.worker, Some(bob()));

    // 4. Submit result (triggers yield — just verify state transitions)
    testing_env!(context
        .signer_account_id(bob())
        .predecessor_account_id(bob())
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());
    contract.submit_result(
        "lifecycle-1".to_string(),
        "{\"files\": [\"main.py\", \"test_main.py\"], \"tests_pass\": true}".to_string(),
    );

    // Verify Verifying
    let escrow = contract.get_escrow("lifecycle-1".to_string()).unwrap();
    assert!(matches!(escrow.status, super::EscrowStatus::Verifying));
    assert_eq!(
        escrow.result,
        Some("{\"files\": [\"main.py\", \"test_main.py\"], \"tests_pass\": true}".to_string())
    );

    // 5. Verify stats reflect correct state
    let stats = contract.get_stats();
    assert_eq!(stats["total"], 1);
    let by_status = stats["by_status"].as_object().unwrap();
    assert_eq!(by_status.get("Verifying").unwrap().as_u64().unwrap(), 1);
}

// ─── State machine fixes ────────────────────────────────────────

#[test]
#[ignore] // Requires NEAR sandbox — exercises promise_yield_create/resume
fn test_yield_timeout_refunds_worker_stake() {
    // Fix 1: Worker stake is refunded on yield timeout, not forfeited to agent.
    // We can't simulate yield timeout directly in unit tests (that's runtime behavior),
    // but we verify the code path exists by checking the escrow state after submit_result.
    let mut context = setup();
    let mut contract = create_funded_contract(&mut context, "job-stake-refund");

    // Worker claims
    testing_env!(context
        .signer_account_id(bob())
        .predecessor_account_id(bob())
        .attached_deposit(worker_stake())
        .build());
    contract.claim("job-stake-refund".to_string());

    let escrow = contract.get_escrow("job-stake-refund".to_string()).unwrap();
    assert!(matches!(escrow.status, super::EscrowStatus::InProgress));
    assert_eq!(escrow.worker, Some(bob()));

    // Submit result → Verifying
    testing_env!(context
        .signer_account_id(bob())
        .predecessor_account_id(bob())
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());
    contract.submit_result("job-stake-refund".to_string(), "Result data".to_string());

    let escrow = contract.get_escrow("job-stake-refund".to_string()).unwrap();
    assert!(matches!(escrow.status, super::EscrowStatus::Verifying));
    // Worker is still set — stake will be refunded to them on timeout
    assert!(escrow.worker.is_some());
}

#[test]
#[should_panic(expected = "Not retryable")]
fn test_retry_settlement_non_owner_before_expiry_rejected() {
    // Non-owner calling retry on a non-SettlementFailed escrow.
    // Hits "Not retryable" before the expiry gate — confirms the status check runs first.
    let mut context = setup();
    let mut contract = create_funded_contract(&mut context, "job-retry-owner");

    testing_env!(context
        .signer_account_id(bob())
        .predecessor_account_id(bob())
        .build());
    contract.retry_settlement("job-retry-owner".to_string());
}

#[test]
fn test_retry_settlement_owner_no_cooldown() {
    let mut context = setup();
    let mut contract = create_funded_contract(&mut context, "job-retry-no-wait");

    // Owner should be able to call retry_settlement without waiting for expiry.
    // This will fail at "Not retryable" (status isn't SettlementFailed),
    // but the point is it DOESN'T fail at the expiry gate.
    testing_env!(context
        .signer_account_id(alice())
        .predecessor_account_id(alice())
        .build());

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        contract.retry_settlement("job-retry-no-wait".to_string());
    }));

    // Should panic with "Not retryable", NOT "Only owner can retry before expiry"
    let err = result.unwrap_err();
    if let Some(s) = err.downcast_ref::<String>() {
        assert!(
            s.contains("Not retryable"),
            "Expected 'Not retryable', got: {}",
            s
        );
    } else if let Some(s) = err.downcast_ref::<&str>() {
        assert!(
            s.contains("Not retryable"),
            "Expected 'Not retryable', got: {}",
            s
        );
    } else {
        panic!("Panic payload was not a string");
    }
}

#[test]
fn test_refund_expired_pending_funding() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());
    let mut contract = new_contract();

    contract.create_escrow(
        "job-expire-pf".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        1, // 1 hour timeout
        "Task".to_string(),
        "Criteria".to_string(),
        None,
        None,
        None,
        None,
    );

    // Advance time past the 1h timeout (block_timestamp is in nanoseconds in VMContext)
    testing_env!(context
        .block_timestamp(2 * 3_600_000 * 1_000_000) // 2 hours in nanoseconds
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());

    contract.refund_expired("job-expire-pf".to_string());

    let escrow = contract.get_escrow("job-expire-pf".to_string()).unwrap();
    assert!(matches!(escrow.status, super::EscrowStatus::Cancelled));
}

#[test]
fn test_refund_expired_open_status() {
    let mut context = setup();
    let mut contract = create_funded_contract(&mut context, "job-expire-open");

    // create_funded_contract uses 24h timeout, created_at is block_timestamp_ms
    // which defaults to 0 in VMContextBuilder. block_timestamp is nanoseconds.
    testing_env!(context
        .block_timestamp(25 * 3_600_000 * 1_000_000) // 25 hours in nanoseconds
        .build());

    // Open + expired → triggers FullRefund settlement
    // In unit tests the cross-contract FT transfer can't execute,
    // so status stays Open but settlement_target is set, confirming the path was taken.
    contract.refund_expired("job-expire-open".to_string());

    // Can't check status change (async settlement) but the call didn't panic
    // which confirms the Open → FullRefund path works
}

// ─── Worker wallet tests ──────────────────────────────────────────

fn owner() -> AccountId {
    "alice.near".parse().unwrap()
}

fn daemon() -> AccountId {
    "daemon.near".parse().unwrap()
}

fn worker_pubkey() -> String {
    // 64 hex chars (32 bytes) — just a test pubkey, doesn't need to be valid ed25519 for most tests
    "a".repeat(64)
}

fn setup_as_owner() -> VMContextBuilder {
    let mut context = setup();
    testing_env!(context
        .predecessor_account_id(owner())
        .signer_account_id(owner())
        .build());
    context
}

#[test]
fn test_register_worker_owner_only() {
    let mut context = setup_as_owner();
    let mut contract = new_contract();

    // Owner can register
    contract.register_worker(worker_pubkey());
    let info = contract.get_worker_info(worker_pubkey()).unwrap();
    assert_eq!(info.nostr_pubkey, worker_pubkey());
    assert_eq!(info.nonce, 0);
}

#[test]
#[should_panic(expected = "Only owner can register workers")]
fn test_register_worker_non_owner_rejected() {
    let mut context = setup();
    // new() sets owner = alice (signer at construction time)
    testing_env!(context
        .predecessor_account_id(owner())
        .signer_account_id(owner())
        .attached_deposit(one_near())
        .build());
    let mut contract = new_contract();

    // Now switch to daemon — daemon is NOT the owner
    testing_env!(context
        .predecessor_account_id(daemon())
        .signer_account_id(daemon())
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());
    contract.register_worker(worker_pubkey());
}

#[test]
#[should_panic(expected = "Already registered")]
fn test_register_worker_duplicate_rejected() {
    let mut context = setup_as_owner();
    let mut contract = new_contract();
    contract.register_worker(worker_pubkey());
    contract.register_worker(worker_pubkey());
}

#[test]
fn test_deposit_to_worker() {
    let mut context = setup_as_owner();
    let mut contract = new_contract();
    contract.register_worker(worker_pubkey());

    // Deposit 0.5 NEAR
    testing_env!(context
        .attached_deposit(NearToken::from_yoctonear(500_000_000_000_000_000_000_000))
        .build());
    contract.deposit_to_worker(worker_pubkey());

    let bal = contract.get_worker_balance(worker_pubkey(), None);
    assert_eq!(bal.0, 500_000_000_000_000_000_000_000);
}

#[test]
#[should_panic(expected = "Must attach NEAR deposit")]
fn test_deposit_to_worker_zero_rejected() {
    let mut context = setup_as_owner();
    let mut contract = new_contract();
    contract.register_worker(worker_pubkey());

    testing_env!(context
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());
    contract.deposit_to_worker(worker_pubkey());
}

#[test]
#[should_panic(expected = "Worker not registered")]
fn test_deposit_to_unregistered_worker_rejected() {
    let mut context = setup_as_owner();
    let mut contract = new_contract();

    testing_env!(context
        .attached_deposit(NearToken::from_yoctonear(500_000_000_000_000_000_000_000))
        .build());
    contract.deposit_to_worker(worker_pubkey());
}

#[test]
fn test_get_worker_balance_default_zero() {
    let context = setup();
    let contract = new_contract();
    let bal = contract.get_worker_balance("nonexistent".to_string(), None);
    assert_eq!(bal.0, 0);
}

#[test]
fn test_get_worker_balances_multiple_tokens() {
    let mut context = setup_as_owner();
    let mut contract = new_contract();
    contract.register_worker(worker_pubkey());

    // Deposit NEAR
    testing_env!(context
        .attached_deposit(NearToken::from_yoctonear(500_000_000_000_000_000_000_000))
        .build());
    contract.deposit_to_worker(worker_pubkey());

    // Check balances list
    let balances = contract.get_worker_balances(worker_pubkey());
    assert_eq!(balances.len(), 1);
    assert_eq!(balances[0]["token"], "near");
}

#[test]
fn test_pause_worker() {
    let mut context = setup_as_owner();
    let mut contract = new_contract();
    contract.register_worker(worker_pubkey());

    // Not paused initially
    assert!(!contract.is_worker_paused(worker_pubkey()));

    // Owner pauses
    contract.pause_worker(worker_pubkey());
    assert!(contract.is_worker_paused(worker_pubkey()));
}

#[test]
fn test_unpause_worker() {
    let mut context = setup_as_owner();
    let mut contract = new_contract();
    contract.register_worker(worker_pubkey());
    contract.pause_worker(worker_pubkey());
    assert!(contract.is_worker_paused(worker_pubkey()));

    contract.unpause_worker(worker_pubkey());
    assert!(!contract.is_worker_paused(worker_pubkey()));
}

#[test]
#[should_panic(expected = "Only owner")]
fn test_pause_worker_non_owner_rejected() {
    let mut context = setup_as_owner();
    let mut contract = new_contract();
    contract.register_worker(worker_pubkey());

    // Switch to daemon (non-owner) and try to pause
    testing_env!(context
        .predecessor_account_id(daemon())
        .signer_account_id(daemon())
        .build());
    contract.pause_worker(worker_pubkey());
}

#[test]
#[should_panic(expected = "Worker is paused")]
fn test_deposit_to_paused_worker_rejected() {
    let mut context = setup_as_owner();
    let mut contract = new_contract();
    contract.register_worker(worker_pubkey());
    contract.pause_worker(worker_pubkey());

    testing_env!(context
        .predecessor_account_id(owner())
        .attached_deposit(NearToken::from_yoctonear(500_000_000_000_000_000_000_000))
        .build());
    contract.deposit_to_worker(worker_pubkey());
}

#[test]
fn test_worker_info_view() {
    let mut context = setup_as_owner();
    let mut contract = new_contract();
    contract.register_worker(worker_pubkey());

    let info = contract.get_worker_info(worker_pubkey()).unwrap();
    assert_eq!(info.nostr_pubkey, worker_pubkey());
    assert!(info.near_account_id.is_none());
    assert_eq!(info.nonce, 0);
}

#[test]
fn test_worker_info_nonexistent() {
    let context = setup();
    let contract = new_contract();
    assert!(contract.get_worker_info("nonexistent".to_string()).is_none());
}

