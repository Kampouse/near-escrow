use near_sdk::test_utils::VMContextBuilder;
use near_sdk::{testing_env, AccountId, NearToken};

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

fn one_near() -> NearToken {
    NearToken::from_yoctonear(1_000_000_000_000_000_000_000_000)
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
    testing_env!(context
        .attached_deposit(one_near())
        .build());

    let mut contract = super::EscrowContract::new();

    contract.create_escrow(
        job_id.to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Build a TODO app".to_string(),
        "Must have CRUD operations and tests".to_string(),
        Some(near_sdk::json_types::U128(100_000)),
        Some(80),
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
    let contract = super::EscrowContract::new();
    assert_eq!(contract.get_owner(), alice());
}

// ─── Create escrow ────────────────────────────────────────────────

#[test]
fn test_create_escrow() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = super::EscrowContract::new();
    contract.create_escrow(
        "job-1".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Build a TODO app".to_string(),
        "Must have CRUD operations".to_string(),
        Some(near_sdk::json_types::U128(50_000)),
        Some(80),
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
    testing_env!(context.attached_deposit(NearToken::from_yoctonear(0)).build());

    let mut contract = super::EscrowContract::new();
    contract.create_escrow(
        "job-2".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
        None,
        None,
    );
}

#[test]
#[should_panic(expected = "Job ID exists")]
fn test_create_escrow_duplicate_id() {
    let mut context = setup();
    testing_env!(context.attached_deposit(NearToken::from_yoctonear(2 * 1_000_000_000_000_000_000_000_000)).build());

    let mut contract = super::EscrowContract::new();

    contract.create_escrow(
        "job-dup".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
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
    );
}

#[test]
#[should_panic(expected = "Verifier fee must be less than amount")]
fn test_create_escrow_fee_too_high() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = super::EscrowContract::new();
    contract.create_escrow(
        "job-fee".to_string(),
        near_sdk::json_types::U128(100),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
        Some(near_sdk::json_types::U128(100)),
        None,
    );
}

// ─── FT funding ───────────────────────────────────────────────────

#[test]
fn test_ft_on_transfer_funding() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = super::EscrowContract::new();
    contract.create_escrow(
        "job-ft".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
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

    let mut contract = super::EscrowContract::new();
    contract.create_escrow(
        "job-wrong".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
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

    let mut contract = super::EscrowContract::new();
    contract.create_escrow(
        "job-amt".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
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

    // Bob claims
    testing_env!(context
        .signer_account_id(bob())
        .predecessor_account_id(bob())
        .attached_deposit(NearToken::from_yoctonear(0))
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
        .attached_deposit(NearToken::from_yoctonear(0))
        .build());
    contract.claim("job-self".to_string());
}

// ─── Cancel ───────────────────────────────────────────────────────

#[test]
fn test_cancel_pending_funding() {
    let mut context = setup();
    testing_env!(context.attached_deposit(one_near()).build());

    let mut contract = super::EscrowContract::new();
    contract.create_escrow(
        "job-cancel".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
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

    let mut contract = super::EscrowContract::new();
    contract.create_escrow(
        "job-wrong-cancel".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task".to_string(),
        "Criteria".to_string(),
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

    let mut contract = super::EscrowContract::new();

    contract.create_escrow(
        "stat-1".to_string(),
        near_sdk::json_types::U128(1_000_000),
        token_contract(),
        24,
        "Task 1".to_string(),
        "Criteria".to_string(),
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
    );

    let stats = contract.get_stats();
    assert_eq!(stats["total"], 2);
}

#[test]
fn test_get_storage_deposit() {
    let context = setup();
    testing_env!(context.build());
    let contract = super::EscrowContract::new();
    let deposit = contract.get_storage_deposit();
    assert_eq!(deposit.0, 1_000_000_000_000_000_000_000_000); // 1 NEAR
}

#[test]
fn test_list_open_empty() {
    let context = setup();
    testing_env!(context.build());
    let contract = super::EscrowContract::new();
    let open = contract.list_open(None, None);
    assert!(open.is_empty());
}
