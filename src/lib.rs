use near_sdk::borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::collections::UnorderedMap;
use near_sdk::json_types::U128;
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::serde_json;
use near_sdk::{
    env, log, near, AccountId, CryptoHash, Gas, GasWeight, NearToken, PanicOnDefault, Promise,
    PromiseError,
};

const GAS_FOR_YIELD_CALLBACK: Gas = Gas::from_tgas(200);
const GAS_FOR_FT_TRANSFER: Gas = Gas::from_tgas(30);
const GAS_FOR_SETTLE_CALLBACK: Gas = Gas::from_tgas(10);
const ONE_YOCTO: NearToken = NearToken::from_yoctonear(1);
const DATA_ID_REGISTER: u64 = 0;

// Storage deposit per escrow — generous overestimate.
// Covers the Escrow struct + UnorderedMap entry overhead.
// Surplus is refunded on settle/cancel.
const STORAGE_DEPOSIT_YOCTO: u128 = 1_000_000_000_000_000_000_000_000; // 1 NEAR

// Worker stake — anti-spam bond. Forfeited to agent on yield timeout
// (worker submitted but never verified). Refunded on successful settlement.
const WORKER_STAKE_YOCTO: u128 = 100_000_000_000_000_000_000_000; // 0.1 NEAR

// Safety timeout for stuck Verifying escrows — 24 hours in milliseconds.
// After this time, the owner can force_cancel_verifying to recover funds.
const VERIFICATION_SAFETY_TIMEOUT_MS: u64 = 24 * 3600 * 1000;

// --- Verifier verdict ---

#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Clone, Debug)]
#[borsh(crate = "near_sdk::borsh")]
#[serde(crate = "near_sdk::serde")]
pub struct VerifierVerdict {
    pub score: u8,
    pub passed: bool,
    pub detail: String,
}

// --- Multi-verifier consensus types ---

#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Clone, Debug)]
#[borsh(crate = "near_sdk::borsh")]
#[serde(crate = "near_sdk::serde")]
pub struct VerifierInfo {
    pub account_id: AccountId,
    /// ed25519 public key (32 bytes hex) for signature verification
    pub public_key: String,
    pub active: bool,
}

#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Clone, Debug)]
#[borsh(crate = "near_sdk::borsh")]
#[serde(crate = "near_sdk::serde")]
pub struct VerifierSignature {
    /// Index into verifier_set
    pub verifier_index: u8,
    /// ed25519 signature (64 bytes)
    pub signature: Vec<u8>,
}

#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Clone, Debug)]
#[borsh(crate = "near_sdk::borsh")]
#[serde(crate = "near_sdk::serde")]
pub struct SignedVerdict {
    /// JSON verdict: {"score": u8, "passed": bool, "detail": String}
    pub verdict_json: String,
    /// ed25519 signatures from verifiers
    pub signatures: Vec<VerifierSignature>,
}

// --- Escrow status ---

#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[borsh(crate = "near_sdk::borsh")]
#[serde(crate = "near_sdk::serde")]
pub enum EscrowStatus {
    PendingFunding,   // Created, waiting for FT deposit
    Open,             // Funded, waiting for worker
    InProgress,       // Worker claimed, doing the job
    Verifying,        // Result submitted, yield active — do NOT refund
    Claimed,          // Passed verification, worker paid
    Refunded,         // Failed verification or timeout, agent refunded
    Cancelled,        // Cancelled before funding or before worker claimed
    SettlementFailed, // FT transfer failed, admin can retry
}

// --- Settlement target (stored during settlement) ---

#[derive(BorshDeserialize, BorshSerialize, Serialize, Clone, PartialEq, Debug)]
#[borsh(crate = "near_sdk::borsh")]
#[serde(crate = "near_sdk::serde")]
pub enum SettlementTarget {
    Claim,      // Pay worker minus verifier fee
    Refund,     // Refund agent minus verifier fee
    FullRefund, // Full refund (timeout or cancel)
}

// --- Escrow mode ---

#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[borsh(crate = "near_sdk::borsh")]
#[serde(crate = "near_sdk::serde")]
pub enum EscrowMode {
    Standard,    // Single worker: claim → submit → verify
    Competitive, // Multiple workers submit, agent designates winner
}

// --- Worker account (internal wallet) ---

#[derive(BorshDeserialize, BorshSerialize, Serialize, Clone)]
#[borsh(crate = "near_sdk::borsh")]
#[serde(crate = "near_sdk::serde")]
pub struct WorkerAccount {
    pub nostr_pubkey: String,
    pub near_account_id: Option<AccountId>,
    pub nonce: u64,
}

#[derive(Serialize, Clone)]
#[serde(crate = "near_sdk::serde")]
pub struct WorkerAccountView {
    pub nostr_pubkey: String,
    pub near_account_id: Option<AccountId>,
    pub nonce: u64,
}

impl From<WorkerAccount> for WorkerAccountView {
    fn from(w: WorkerAccount) -> Self {
        WorkerAccountView {
            nostr_pubkey: w.nostr_pubkey,
            near_account_id: w.near_account_id,
            nonce: w.nonce,
        }
    }
}

// --- Competitive submission ---

#[derive(BorshDeserialize, BorshSerialize, Serialize, Clone)]
#[borsh(crate = "near_sdk::borsh")]
#[serde(crate = "near_sdk::serde")]
pub struct Submission {
    pub worker: AccountId,
    pub result: String,
    pub stake: U128, // Anti-spam bond, refunded to non-winners on designate
    pub worker_pubkey: Option<String>, // Internal wallet identity (ed25519 hex)
}

// Max retries for failed settlements before auto-cancel
const MAX_SETTLEMENT_RETRIES: u8 = 5;

// --- Escrow record (internal) ---

#[derive(BorshDeserialize, BorshSerialize, Serialize, Clone)]
#[borsh(crate = "near_sdk::borsh")]
#[serde(crate = "near_sdk::serde")]
pub struct Escrow {
    pub job_id: String,
    pub agent: AccountId,
    pub worker: Option<AccountId>,
    pub amount: U128,
    pub token: AccountId,
    pub created_at: u64,
    pub timeout_ms: u64,
    pub status: EscrowStatus,
    pub task_description: String,
    pub criteria: String,
    pub verifier_fee: U128,
    pub result: Option<String>,
    pub score_threshold: u8,
    pub verdict: Option<VerifierVerdict>,
    // Internal — not exposed in views
    pub data_id: Option<CryptoHash>,
    pub settlement_target: Option<SettlementTarget>,
    pub worker_stake: Option<U128>, // Anti-spam bond (0.1 NEAR), refunded on settle
    pub yield_consumed: bool,       // Guard against double resume_verification
    // Internal wallet — worker identity (ed25519 hex pubkey)
    pub worker_pubkey: Option<String>,
    // Competitive mode fields
    pub mode: EscrowMode,
    pub max_submissions: Option<u32>,
    pub submissions: Vec<Submission>,  // Competitive: all worker submissions
    pub winner_idx: Option<u32>,       // Competitive: index of winning submission
    pub deadline_block: Option<u64>,   // Competitive: optional submission deadline
    pub retry_count: u8,               // Settlement retry counter (auto-cancel after MAX_SETTLEMENT_RETRIES)
}

// --- Escrow view (public, no internal fields) ---

#[derive(Serialize, Clone)]
#[serde(crate = "near_sdk::serde")]
pub struct EscrowView {
    pub job_id: String,
    pub agent: AccountId,
    pub worker: Option<AccountId>,
    pub amount: U128,
    pub token: AccountId,
    pub created_at: u64,
    pub timeout_ms: u64,
    pub status: EscrowStatus,
    pub task_description: String,
    pub criteria: String,
    pub verifier_fee: U128,
    pub result: Option<String>,
    pub score_threshold: u8,
    pub verdict: Option<VerifierVerdict>,
    pub mode: EscrowMode,
    pub max_submissions: Option<u32>,
    pub submission_count: u32,
    pub winner_idx: Option<u32>,
    pub worker_pubkey: Option<String>,
    pub retry_count: u8,
}

impl From<Escrow> for EscrowView {
    fn from(e: Escrow) -> Self {
        EscrowView {
            job_id: e.job_id,
            agent: e.agent,
            worker: e.worker,
            amount: e.amount,
            token: e.token,
            created_at: e.created_at,
            timeout_ms: e.timeout_ms,
            status: e.status,
            task_description: e.task_description,
            criteria: e.criteria,
            verifier_fee: e.verifier_fee,
            result: e.result,
            score_threshold: e.score_threshold,
            verdict: e.verdict,
            mode: e.mode,
            max_submissions: e.max_submissions,
            submission_count: e.submissions.len() as u32,
            winner_idx: e.winner_idx,
            worker_pubkey: e.worker_pubkey,
            retry_count: e.retry_count,
        }
    }
}

// --- Helpers ---

fn emit_event(event: &str, data: &serde_json::Value) {
    // NEP-297 compliant event format
    env::log_str(&format!(
        "EVENT_JSON:{}",
        &serde_json::json!({
            "standard": "escrow",
            "version": "3.2.0",
            "event": event,
            "data": [data],
        })
    ));
}

fn ft_transfer_promise(token: &AccountId, receiver: AccountId, amount: u128) -> Promise {
    let args = serde_json::json!({
        "receiver_id": receiver,
        "amount": U128(amount),
    });
    Promise::new(token.clone()).function_call(
        "ft_transfer".to_string(),
        serde_json::to_vec(&args).expect("ft_transfer args serialization failed"),
        ONE_YOCTO,
        GAS_FOR_FT_TRANSFER,
    )
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn hex_decode(hex: &str) -> Vec<u8> {
    assert!(hex.len() % 2 == 0, "Invalid hex length");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("Invalid hex char"))
        .collect()
}

/// Non-panicking hex decode — returns None on invalid input.
fn hex_decode_safe(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        match u8::from_str_radix(&hex[i..i + 2], 16) {
            Ok(b) => bytes.push(b),
            Err(_) => return None,
        }
    }
    Some(bytes)
}

/// Sentinel token ID for native NEAR in internal balances.
const NEAR_TOKEN_ID: &str = "near";

fn balance_key(worker_pubkey: &str, token: &str) -> String {
    format!("{}:{}", worker_pubkey, token)
}

fn verify_worker_signature(pubkey_hex: &str, message: &str, signature: &[u8]) {
    let pk_bytes = hex_decode(pubkey_hex);
    assert_eq!(pk_bytes.len(), 32, "Invalid pubkey: expected 32 bytes, got {}", pk_bytes.len());
    assert_eq!(signature.len(), 64, "Invalid signature: expected 64 bytes, got {}", signature.len());
    let pk: [u8; 32] = pk_bytes.try_into().expect("pk len checked");
    let sig: [u8; 64] = signature.try_into().expect("sig len checked");
    assert!(
        env::ed25519_verify(&sig, message.as_bytes(), &pk),
        "Invalid worker signature"
    );
}

/// Credit a worker's internal balance. Returns the previous balance.
fn credit_balance(
    balances: &mut UnorderedMap<String, U128>,
    worker_pubkey: &str,
    token: &str,
    amount: u128,
) {
    let key = balance_key(worker_pubkey, token);
    let current = balances.get(&key).unwrap_or(U128(0)).0;
    balances.insert(&key, &U128(current.saturating_add(amount)));
}

/// Debit a worker's internal balance. Panics if insufficient.
fn debit_balance(
    balances: &mut UnorderedMap<String, U128>,
    worker_pubkey: &str,
    token: &str,
    amount: u128,
) {
    let key = balance_key(worker_pubkey, token);
    let current = balances.get(&key).unwrap_or(U128(0)).0;
    assert!(current >= amount, "Insufficient internal balance: have {}, need {}", current, amount);
    balances.insert(&key, &U128(current.saturating_sub(amount)));
}

// --- Cached stats (O(1) get_stats) ---

#[derive(BorshDeserialize, BorshSerialize, Serialize, Clone, Debug, Default)]
#[borsh(crate = "near_sdk::borsh")]
#[serde(crate = "near_sdk::serde")]
pub struct EscrowStats {
    pub total_created: u64,
    pub pending_funding: u64,
    pub open: u64,
    pub in_progress: u64,
    pub verifying: u64,
    pub claimed: u64,
    pub refunded: u64,
    pub cancelled: u64,
    pub settlement_failed: u64,
}

impl EscrowStats {
    fn total(&self) -> u64 {
        self.pending_funding
            .saturating_add(self.open)
            .saturating_add(self.in_progress)
            .saturating_add(self.verifying)
            .saturating_add(self.claimed)
            .saturating_add(self.refunded)
            .saturating_add(self.cancelled)
            .saturating_add(self.settlement_failed)
    }
}

// --- Contract ---

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct EscrowContract {
    owner: AccountId,
    /// Two-step owner transfer: new owner must accept
    pending_owner: Option<AccountId>,
    /// Global pause switch — blocks create_escrow, claim, submit_result when true
    paused: bool,
    escrows: UnorderedMap<String, Escrow>,
    /// Multi-verifier consensus set.
    verifier_set: Vec<VerifierInfo>,
    /// Min signatures needed for consensus (default 2).
    consensus_threshold: u8,
    /// Reverse index: hex(data_id) → job_id — O(1) lookup for resume_verification.
    data_id_index: UnorderedMap<String, String>,
    /// Configurable storage deposit per escrow (default 1 NEAR).
    storage_deposit_yocto: u128,
    /// Configurable worker anti-spam stake (default 0.1 NEAR).
    worker_stake_yocto: u128,
    /// Internal wallet: worker accounts keyed by ed25519 pubkey hex.
    workers: UnorderedMap<String, WorkerAccount>,
    /// Internal wallet: balances keyed by "pubkey_hex:token_id".
    /// token_id is AccountId string for FT tokens, "near" for native NEAR.
    balances: UnorderedMap<String, U128>,
    /// Paused/banned worker pubkeys — admin can pause/unpause.
    /// Paused workers cannot claim, submit, or withdraw.
    paused_workers: UnorderedMap<String, bool>,
    /// Whitelist of accepted FT token contracts. Empty = accept all (dangerous).
    allowed_tokens: Vec<AccountId>,
    /// Cached aggregate stats — O(1) reads, updated on every state transition.
    stats: EscrowStats,
}

/// Helper: decrement the counter for old_status, increment for new_status.
fn transition_stats(stats: &mut EscrowStats, old_status: &EscrowStatus, new_status: &EscrowStatus) {
    // Decrement old
    match old_status {
        EscrowStatus::PendingFunding => stats.pending_funding = stats.pending_funding.saturating_sub(1),
        EscrowStatus::Open => stats.open = stats.open.saturating_sub(1),
        EscrowStatus::InProgress => stats.in_progress = stats.in_progress.saturating_sub(1),
        EscrowStatus::Verifying => stats.verifying = stats.verifying.saturating_sub(1),
        EscrowStatus::Claimed => stats.claimed = stats.claimed.saturating_sub(1),
        EscrowStatus::Refunded => stats.refunded = stats.refunded.saturating_sub(1),
        EscrowStatus::Cancelled => stats.cancelled = stats.cancelled.saturating_sub(1),
        EscrowStatus::SettlementFailed => stats.settlement_failed = stats.settlement_failed.saturating_sub(1),
    }
    // Increment new
    match new_status {
        EscrowStatus::PendingFunding => stats.pending_funding = stats.pending_funding.saturating_add(1),
        EscrowStatus::Open => stats.open = stats.open.saturating_add(1),
        EscrowStatus::InProgress => stats.in_progress = stats.in_progress.saturating_add(1),
        EscrowStatus::Verifying => stats.verifying = stats.verifying.saturating_add(1),
        EscrowStatus::Claimed => stats.claimed = stats.claimed.saturating_add(1),
        EscrowStatus::Refunded => stats.refunded = stats.refunded.saturating_add(1),
        EscrowStatus::Cancelled => stats.cancelled = stats.cancelled.saturating_add(1),
        EscrowStatus::SettlementFailed => stats.settlement_failed = stats.settlement_failed.saturating_add(1),
    }
}


// String length caps — prevent state bloat / gas-exhaustion attacks
const MAX_JOB_ID_LEN: usize = 128;
const MAX_TASK_DESCRIPTION_LEN: usize = 2048;
const MAX_CRITERIA_LEN: usize = 2048;
const MAX_RESULT_LEN: usize = 8192;
const MAX_COMPETITIVE_RESULT_LEN: usize = 2048;

#[near]
impl EscrowContract {
    #[init]
    pub fn new(
        verifier_set: Vec<VerifierInfo>,
        consensus_threshold: Option<u8>,
        allowed_tokens: Vec<AccountId>,
    ) -> Self {
        assert!(!env::state_exists(), "Contract already initialized");
        let owner = env::signer_account_id();

        // Validate verifiers
        assert!(!verifier_set.is_empty(), "At least 1 verifier required");
        for v in &verifier_set {
            assert!(v.public_key.len() == 64, "Invalid pubkey length for {}", v.account_id);
        }
        let threshold = consensus_threshold.unwrap_or(2);
        let active_count = verifier_set.iter().filter(|v| v.active).count();
        assert!(threshold as usize <= active_count, "Threshold exceeds active verifiers");
        assert!(threshold >= 1, "Threshold must be >= 1");

        Self {
            owner,
            pending_owner: None,
            paused: false,
            escrows: UnorderedMap::new(b"e"),
            verifier_set,
            consensus_threshold: threshold,
            data_id_index: UnorderedMap::new(b"d"),
            storage_deposit_yocto: STORAGE_DEPOSIT_YOCTO,
            worker_stake_yocto: WORKER_STAKE_YOCTO,
            workers: UnorderedMap::new(b"w"),
            balances: UnorderedMap::new(b"b"),
            paused_workers: UnorderedMap::new(b"p"),
            allowed_tokens,
            stats: EscrowStats::default(),
        }
    }

    // ========================================
    // Admin: multi-verifier management
    // ========================================

    /// Add a verifier to the consensus set. Owner-only.
    pub fn add_verifier(&mut self, account_id: AccountId, public_key: String) {
        assert!(
            env::predecessor_account_id() == self.owner,
            "Only owner can manage verifiers"
        );
        assert!(public_key.len() == 64, "Public key must be 64 hex chars");
        // Check not already in set
        for v in &self.verifier_set {
            assert!(v.account_id != account_id, "Verifier already in set");
        }
        self.verifier_set.push(VerifierInfo {
            account_id,
            public_key,
            active: true,
        });
    }

    /// Remove a verifier (deactivate — don't shift indices). Owner-only.
    pub fn deactivate_verifier(&mut self, index: u8) {
        assert!(
            env::predecessor_account_id() == self.owner,
            "Only owner"
        );
        let idx = index as usize;
        assert!(idx < self.verifier_set.len(), "Invalid verifier index");
        self.verifier_set[idx].active = false;
    }

    /// Update consensus threshold. Owner-only.
    pub fn set_consensus_threshold(&mut self, threshold: u8) {
        assert!(
            env::predecessor_account_id() == self.owner,
            "Only owner"
        );
        let active_count = self.verifier_set.iter().filter(|v| v.active).count();
        assert!(threshold as usize <= active_count, "Threshold exceeds active verifiers");
        assert!(threshold >= 1, "Threshold must be >= 1");
        self.consensus_threshold = threshold;
    }

    /// Get current verifier set
    pub fn get_verifier_set(&self) -> Vec<VerifierInfo> {
        self.verifier_set.clone()
    }

    /// Check if multi-verifier mode is active
    pub fn is_multi_verifier(&self) -> bool {
        !self.verifier_set.is_empty()
    }

    // ========================================
    // Admin: owner transfer (2-step)
    // ========================================

    /// Step 1: Propose new owner. Current owner only.
    pub fn propose_owner(&mut self, new_owner: AccountId) {
        assert_eq!(env::predecessor_account_id(), self.owner, "Only owner");
        emit_event("owner_proposed", &serde_json::json!({"new_owner": new_owner.to_string()}));
        self.pending_owner = Some(new_owner);
    }

    /// Step 2: Accept ownership. Must be called by the proposed new owner.
    pub fn accept_owner(&mut self) {
        let caller = env::predecessor_account_id();
        let pending = self.pending_owner.take();
        match pending {
            Some(ref po) if po == &caller => {
                self.owner = caller.clone();
                emit_event("owner_transferred", &serde_json::json!({"new_owner": caller}));
            }
            Some(po) => {
                // Wrong caller — put it back
                self.pending_owner = Some(po);
                panic!("Not the proposed owner");
            }
            None => panic!("No pending owner transfer"),
        }
    }

    /// Get pending owner (if any)
    pub fn get_pending_owner(&self) -> Option<AccountId> {
        self.pending_owner.clone()
    }

    // ========================================
    // Admin: global pause
    // ========================================

    /// Pause all escrow operations (create, claim, submit). Owner-only.
    /// Existing escrows continue through verification/settlement.
    pub fn pause(&mut self) {
        assert_eq!(env::predecessor_account_id(), self.owner, "Only owner");
        self.paused = true;
        emit_event("contract_paused", &serde_json::json!({}));
    }

    /// Unpause the contract. Owner-only.
    pub fn unpause(&mut self) {
        assert_eq!(env::predecessor_account_id(), self.owner, "Only owner");
        self.paused = false;
        emit_event("contract_unpaused", &serde_json::json!({}));
    }

    /// Check if contract is paused
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    // ========================================
    // Admin: token whitelist
    // ========================================

    /// Add a token to the whitelist. Owner-only. Empty whitelist = accept all.
    pub fn add_allowed_token(&mut self, token: AccountId) {
        assert_eq!(env::predecessor_account_id(), self.owner, "Only owner");
        if !self.allowed_tokens.contains(&token) {
            self.allowed_tokens.push(token);
        }
    }

    /// Remove a token from the whitelist. Owner-only.
    pub fn remove_allowed_token(&mut self, token: AccountId) {
        assert_eq!(env::predecessor_account_id(), self.owner, "Only owner");
        self.allowed_tokens.retain(|t| t != &token);
    }

    /// Get list of allowed tokens. Empty = all tokens accepted.
    pub fn get_allowed_tokens(&self) -> Vec<AccountId> {
        self.allowed_tokens.clone()
    }

    fn is_token_allowed(&self, token: &AccountId) -> bool {
        self.allowed_tokens.is_empty() || self.allowed_tokens.contains(token)
    }

    // ========================================
    // Internal wallet: worker registration & deposits
    // ========================================

    /// Register a worker by their Nostr ed25519 pubkey.
    /// Owner-only: the daemon controls worker registration.
    /// Workers are auto-registered on first claim_for/submit_result_for if not yet registered.
    pub fn register_worker(&mut self, nostr_pubkey: String) {
        assert!(
            env::predecessor_account_id() == self.owner,
            "Only owner can register workers"
        );
        assert!(!nostr_pubkey.is_empty(), "Pubkey required");
        assert!(
            nostr_pubkey.len() == 64,
            "Pubkey must be 64 hex chars (32 bytes), got {}",
            nostr_pubkey.len()
        );
        assert!(
            self.workers.get(&nostr_pubkey).is_none(),
            "Already registered"
        );
        assert!(
            self.paused_workers.get(&nostr_pubkey).is_none(),
            "Worker is paused/banned"
        );
        self.workers.insert(&nostr_pubkey, &WorkerAccount {
            nostr_pubkey: nostr_pubkey.clone(),
            near_account_id: None,
            nonce: 0,
        });
        emit_event(
            "worker_registered",
            &serde_json::json!({ "worker_pubkey": nostr_pubkey }),
        );
    }

    /// Deposit native NEAR into a worker's internal balance.
    /// Anyone can call (daemon fronts stakes, worker self-funds, etc).
    #[payable]
    pub fn deposit_to_worker(&mut self, worker_pubkey: String) {
        let amount = env::attached_deposit().as_yoctonear();
        assert!(amount > 0, "Must attach NEAR deposit");
        assert!(
            self.workers.get(&worker_pubkey).is_some(),
            "Worker not registered — call register_worker first"
        );
        assert!(
            self.paused_workers.get(&worker_pubkey).is_none(),
            "Worker is paused"
        );
        credit_balance(&mut self.balances, &worker_pubkey, NEAR_TOKEN_ID, amount);
        emit_event(
            "worker_deposit",
            &serde_json::json!({
                "worker_pubkey": worker_pubkey,
                "amount": amount.to_string(),
                "token": "near",
            }),
        );
    }

    /// Worker withdraws from internal balance to an external NEAR account.
    /// Worker signs: "withdraw:{token}:{amount}:{to}:{nonce}" with their Nostr key.
    /// Anyone can relay the signed message (daemon, worker themselves, etc).
    pub fn withdraw(
        &mut self,
        worker_pubkey: String,
        token: String,
        amount: U128,
        to: AccountId,
        signature: Vec<u8>,
    ) {
        assert!(
            self.paused_workers.get(&worker_pubkey).is_none(),
            "Worker is paused"
        );
        let mut worker = self.workers.get(&worker_pubkey).expect("Worker not registered");
        let message = format!("{}:withdraw:{}:{}:{}:{}", env::current_account_id(), token, amount.0, to, worker.nonce);
        verify_worker_signature(&worker_pubkey, &message, &signature);

        // Debit internal balance
        debit_balance(&mut self.balances, &worker_pubkey, &token, amount.0);

        // Increment nonce
        worker.nonce = worker.nonce.saturating_add(1);
        self.workers.insert(&worker_pubkey, &worker);

        // Execute FT transfer (or NEAR transfer for native)
        if token == NEAR_TOKEN_ID {
            Promise::new(to.clone()).transfer(NearToken::from_yoctonear(amount.0));
        } else {
            let token_account: AccountId = token.parse().expect("Invalid token account ID");
            let _ = ft_transfer_promise(&token_account, to.clone(), amount.0);
        }

        emit_event(
            "worker_withdraw",
            &serde_json::json!({
                "worker_pubkey": worker_pubkey,
                "token": token.to_string(),
                "amount": amount.0.to_string(),
                "to": to.to_string(),
            }),
        );
    }

    /// Link a NEAR account to a worker for direct withdrawal calls.
    /// Worker signs: "link:{near_account_id}:{nonce}" with their Nostr key.
    pub fn link_near_account(
        &mut self,
        worker_pubkey: String,
        near_account_id: AccountId,
        signature: Vec<u8>,
    ) {
        let mut worker = self.workers.get(&worker_pubkey).expect("Worker not registered");
        let message = format!("link:{}:{}", near_account_id, worker.nonce);
        verify_worker_signature(&worker_pubkey, &message, &signature);
        worker.nonce = worker.nonce.saturating_add(1);
        worker.near_account_id = Some(near_account_id.clone());
        self.workers.insert(&worker_pubkey, &worker);
        emit_event(
            "near_account_linked",
            &serde_json::json!({
                "worker_pubkey": worker_pubkey,
                "near_account_id": near_account_id,
            }),
        );
    }

    // ========================================
    // Internal wallet: claim & submit via pubkey
    // ========================================

    /// Worker claims an open escrow via Nostr key signature.
    /// Stake is deducted from internal NEAR balance (no attached deposit needed).
    /// Daemon relays the signed message on behalf of the worker.
    pub fn claim_for(&mut self, job_id: String, worker_pubkey: String, worker_signature: Vec<u8>) {
        assert!(!self.paused, "Contract is paused");
        // Auto-register if needed
        if self.workers.get(&worker_pubkey).is_none() {
            self.workers.insert(&worker_pubkey, &WorkerAccount {
                nostr_pubkey: worker_pubkey.clone(),
                near_account_id: None,
                nonce: 0,
            });
        }

        // Pause check
        assert!(
            self.paused_workers.get(&worker_pubkey).is_none(),
            "Worker is paused"
        );

        // Verify worker identity with nonce + contract domain separator
        let worker = self.workers.get(&worker_pubkey).expect("Worker not registered");
        let message = format!("{}:claim:{}:{}", env::current_account_id(), job_id, worker.nonce);
        verify_worker_signature(&worker_pubkey, &message, &worker_signature);

        // Increment nonce after successful verification
        let mut worker = worker;
        worker.nonce = worker.nonce.saturating_add(1);
        self.workers.insert(&worker_pubkey, &worker);

        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");
        assert!(escrow.status == EscrowStatus::Open, "Escrow not open");
        assert!(escrow.worker.is_none() && escrow.worker_pubkey.is_none(), "Already claimed");
        assert!(
            worker_pubkey != escrow.agent.to_string(),
            "Agent cannot claim own escrow"
        );

        // Deduct stake from internal NEAR balance
        debit_balance(&mut self.balances, &worker_pubkey, NEAR_TOKEN_ID, self.worker_stake_yocto);

        escrow.worker_pubkey = Some(worker_pubkey.clone());
        escrow.worker = Some(env::signer_account_id()); // daemon/relayer as fallback
        escrow.worker_stake = Some(U128(self.worker_stake_yocto));
        escrow.status = EscrowStatus::InProgress;
        transition_stats(&mut self.stats, &EscrowStatus::Open, &EscrowStatus::InProgress);
        self.escrows.insert(&job_id, &escrow);

        emit_event(
            "escrow_claimed_by_worker",
            &serde_json::json!({
                "job_id": job_id,
                "worker_pubkey": worker_pubkey,
            }),
        );
    }

    /// Worker submits result via Nostr key signature.
    /// For standard mode: must be the assigned worker (claimed via claim_for).
    /// For competitive mode: adds submission, deducts stake from internal balance.
    pub fn submit_result_for(
        &mut self,
        job_id: String,
        result: String,
        worker_pubkey: String,
        worker_signature: Vec<u8>,
    ) {
        assert!(!self.paused, "Contract is paused");
        assert!(!result.is_empty(), "Result cannot be empty");
        assert!(
            result.len() <= MAX_RESULT_LEN,
            "Result too long (max {} bytes)",
            MAX_RESULT_LEN
        );

        // Auto-register if needed
        if self.workers.get(&worker_pubkey).is_none() {
            self.workers.insert(&worker_pubkey, &WorkerAccount {
                nostr_pubkey: worker_pubkey.clone(),
                near_account_id: None,
                nonce: 0,
            });
        }

        // Pause check
        assert!(
            self.paused_workers.get(&worker_pubkey).is_none(),
            "Worker is paused"
        );

        // Verify worker identity with nonce + contract domain separator
        let worker = self.workers.get(&worker_pubkey).expect("Worker not registered");
        let message = format!("{}:submit_result:{}:{}", env::current_account_id(), job_id, worker.nonce);
        verify_worker_signature(&worker_pubkey, &message, &worker_signature);

        // Increment nonce after successful verification
        let mut worker = worker;
        worker.nonce = worker.nonce.saturating_add(1);
        self.workers.insert(&worker_pubkey, &worker);

        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");

        match escrow.mode {
            EscrowMode::Competitive => {
                assert!(
                    escrow.status == EscrowStatus::Open,
                    "Competitive: escrow must be Open"
                );
                assert!(
                    result.len() <= MAX_COMPETITIVE_RESULT_LEN,
                    "Competitive result too long (max {} bytes)",
                    MAX_COMPETITIVE_RESULT_LEN
                );
                if let Some(deadline) = escrow.deadline_block {
                    assert!(
                        env::block_height() <= deadline,
                        "Submission deadline passed"
                    );
                }
                // Deduct stake from internal NEAR balance
                debit_balance(
                    &mut self.balances,
                    &worker_pubkey,
                    NEAR_TOKEN_ID,
                    self.worker_stake_yocto,
                );
                // Idempotent
                if escrow.submissions.iter().any(|s| s.worker_pubkey.as_ref() == Some(&worker_pubkey)) {
                    // Refund the stake we just deducted
                    credit_balance(&mut self.balances, &worker_pubkey, NEAR_TOKEN_ID, self.worker_stake_yocto);
                    return;
                }
                let max = escrow.max_submissions.unwrap_or(u32::MAX);
                assert!(
                    (escrow.submissions.len() as u32) < max,
                    "Max submissions reached"
                );
                escrow.submissions.push(Submission {
                    worker: env::signer_account_id(),
                    result: result.clone(),
                    stake: U128(self.worker_stake_yocto),
                    worker_pubkey: Some(worker_pubkey.clone()),
                });
                self.escrows.insert(&job_id, &escrow);
                emit_event(
                    "competitive_submission",
                    &serde_json::json!({
                        "job_id": job_id,
                        "worker_pubkey": worker_pubkey,
                        "submission_count": escrow.submissions.len(),
                    }),
                );
                return; // Stay Open — wait for designate_winner
            }
            EscrowMode::Standard => {
                // Idempotent guard
                if escrow.status == EscrowStatus::Verifying
                    && escrow.worker_pubkey.as_ref() == Some(&worker_pubkey)
                    && escrow.data_id.is_some()
                {
                    return;
                }

                assert!(escrow.status == EscrowStatus::InProgress, "Not in progress");
                assert_eq!(
                    escrow.worker_pubkey.as_ref(),
                    Some(&worker_pubkey),
                    "Not the worker"
                );

                escrow.result = Some(result);
            }
        }

        // Create yield promise (same as existing submit_result)
        let callback_args = serde_json::to_vec(&serde_json::json!({"job_id": job_id}))
            .expect("callback args serialization failed");
        let _promise = env::promise_yield_create(
            "verification_callback",
            &callback_args,
            GAS_FOR_YIELD_CALLBACK,
            GasWeight(0),
            DATA_ID_REGISTER,
        );
        let data_id_bytes = env::read_register(DATA_ID_REGISTER)
            .expect("data_id register not set — promise_yield_create failed");
        let data_id: CryptoHash = data_id_bytes
            .as_slice()
            .try_into()
            .expect("data_id must be 32 bytes");

        escrow.data_id = Some(data_id);
        escrow.status = EscrowStatus::Verifying;
        transition_stats(
            &mut self.stats,
            &EscrowStatus::InProgress,
            &EscrowStatus::Verifying,
        );
        self.escrows.insert(&job_id, &escrow);
        self.data_id_index.insert(&hex_encode(data_id.as_ref()), &job_id);

        emit_event(
            "result_submitted",
            &serde_json::json!({
                "job_id": job_id,
                "data_id": hex_encode(data_id.as_ref()),
                "worker_pubkey": worker_pubkey,
            }),
        );
    }

    // ========================================
    // 1. Agent creates escrow (unfunded)
    // ========================================

    /// Creates an escrow in PendingFunding state.
    /// Requires attached NEAR deposit for storage (1 NEAR, surplus refunded on settle).
    /// Agent must then call ft_transfer_call(token, this_contract, amount, job_id) to fund it.
    #[payable]
    pub fn create_escrow(
        &mut self,
        job_id: String,
        amount: U128,
        token: AccountId,
        timeout_hours: u64,
        task_description: String,
        criteria: String,
        verifier_fee: Option<U128>,
        score_threshold: Option<u8>,
        max_submissions: Option<u32>,
        deadline_block: Option<u64>,
    ) {
        assert!(!self.paused, "Contract is paused");
        let agent = env::signer_account_id();
        assert!(!job_id.is_empty(), "Job ID required");
        assert!(job_id.len() <= MAX_JOB_ID_LEN, "Job ID too long (max {} bytes)", MAX_JOB_ID_LEN);
        assert!(self.is_token_allowed(&token), "Token not allowed");
        assert!(amount.0 > 0, "Amount must be > 0");
        assert!(
            timeout_hours <= 8760,
            "timeout_hours must be 0-8760 (instant to 1 year)"
        );
        assert!(self.escrows.get(&job_id).is_none(), "Job ID exists");

        let vfee = verifier_fee.unwrap_or(U128(0));
        assert!(vfee.0 < amount.0, "Verifier fee must be less than amount");

        // Validate score_threshold is in valid range [0, 100]
        if let Some(st) = score_threshold {
            assert!(st <= 100, "score_threshold must be <= 100, got {}", st);
        }

        // String length caps — validate BEFORE moving into struct
        assert!(
            task_description.len() <= MAX_TASK_DESCRIPTION_LEN,
            "Task description too long (max {} bytes)",
            MAX_TASK_DESCRIPTION_LEN
        );
        assert!(
            !criteria.is_empty(),
            "Criteria required — prevents vague tasks"
        );
        assert!(
            criteria.len() <= MAX_CRITERIA_LEN,
            "Criteria too long (max {} bytes)",
            MAX_CRITERIA_LEN
        );

        // Storage staking: require deposit, refund surplus on settle/cancel
        let attached = env::attached_deposit().as_yoctonear();
        assert!(
            attached >= self.storage_deposit_yocto,
            "Insufficient storage deposit: attach at least 1 NEAR"
        );

        let mode = if max_submissions.is_some() {
            EscrowMode::Competitive
        } else {
            EscrowMode::Standard
        };

        let escrow = Escrow {
            job_id: job_id.clone(),
            agent,
            worker: None,
            amount,
            token,
            created_at: env::block_timestamp_ms(),
            timeout_ms: timeout_hours * 3_600_000,
            status: EscrowStatus::PendingFunding,
            task_description,
            criteria,
            verifier_fee: vfee,
            score_threshold: score_threshold.unwrap_or(80),
            result: None,
            verdict: None,
            data_id: None,
            settlement_target: None,
            worker_stake: None,
            yield_consumed: false,
            worker_pubkey: None,
            mode,
            max_submissions,
            submissions: Vec::new(),
            winner_idx: None,
            deadline_block,
            retry_count: 0,
        };

        self.escrows.insert(&job_id, &escrow);
        self.stats.total_created += 1;
        self.stats.pending_funding += 1;

        emit_event(
            "escrow_created",
            &serde_json::json!({
                "job_id": job_id,
                "agent": escrow.agent,
                "amount": amount.0.to_string(),
                "token": escrow.token,
                "task": escrow.task_description,
            }),
        );
    }

    // ========================================
    // 2. Fund via ft_transfer_call → ft_on_transfer
    // ========================================

    /// Called by the FT contract when agent does:
    ///   ft_transfer_call(escrow_contract, amount, job_id)
    ///
    /// Verifies sender, token, amount match the pending escrow.
    /// Transitions escrow from PendingFunding → Open.
    ///
    /// Returns U128(0) to accept all tokens, or U128(amount) to reject.
    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> U128 {
        let token_contract = env::predecessor_account_id();

        // Reject tokens not on the whitelist
        if !self.is_token_allowed(&token_contract) {
            return U128(amount.0);
        }

        let job_id = msg;

        let mut escrow = match self.escrows.get(&job_id) {
            Some(e) => e,
            None => return U128(amount.0), // No matching escrow — reject
        };

        // Strict validation: sender must be agent, token must match, amount must match
        if sender_id != escrow.agent {
            return U128(amount.0);
        }
        if token_contract != escrow.token {
            return U128(amount.0);
        }
        if amount.0 != escrow.amount.0 {
            return U128(amount.0);
        }
        if escrow.status != EscrowStatus::PendingFunding {
            return U128(amount.0);
        }

        escrow.status = EscrowStatus::Open;
        transition_stats(&mut self.stats, &EscrowStatus::PendingFunding, &EscrowStatus::Open);
        self.escrows.insert(&job_id, &escrow);

        emit_event(
            "escrow_funded",
            &serde_json::json!({
                "job_id": job_id,
                "amount": amount.0.to_string(),
            }),
        );

        U128(0) // Accept all
    }

    // ========================================
    // 3. Worker claims
    // ========================================

    /// Worker (found task via Nostr/FastNear) claims the job.
    /// Agent cannot claim their own escrow.
    /// Requires 0.1 NEAR attached deposit as anti-spam bond.
    /// Bond is refunded on successful settlement, forfeited to agent on timeout.
    /// Worker claims an open escrow. Transitions Open → InProgress.
    /// Requires 0.1 NEAR attached deposit as anti-spam bond.
    #[payable]
    pub fn claim(&mut self, job_id: String) {
        assert!(!self.paused, "Contract is paused");
        let caller = env::signer_account_id();
        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");
        assert!(escrow.status == EscrowStatus::Open, "Escrow not open");
        assert!(escrow.worker.is_none(), "Already claimed");
        assert_ne!(caller, escrow.agent, "Agent cannot claim own escrow");

        // Require anti-spam stake
        let attached = env::attached_deposit().as_yoctonear();
        assert!(
            attached >= self.worker_stake_yocto,
            "Worker stake required: attach at least 0.1 NEAR"
        );

        escrow.worker = Some(caller.clone());
        escrow.worker_stake = Some(U128(attached));
        let old_status = escrow.status;
        escrow.status = EscrowStatus::InProgress;
        self.escrows.insert(&job_id, &escrow);
        transition_stats(&mut self.stats, &old_status, &EscrowStatus::InProgress);

        emit_event(
            "escrow_claimed_by_worker",
            &serde_json::json!({
                "job_id": job_id,
                "worker": caller,
            }),
        );
    }

    // ========================================
    // 4. Worker submits result → yield
    // ========================================

    /// Worker submits result — triggers yield for LLM verification.
    /// Verifier service watches for the `result_submitted` event (contains data_id),
    /// scores the work, then calls promise_yield_resume(data_id, payload).
    /// Worker submits their work result, creating a yield promise for async verification.
    /// In standard mode: transitions InProgress → Verifying.
    /// In competitive mode: appends submission, stays Open until designate_winner.
    #[payable]
    pub fn submit_result(&mut self, job_id: String, result: String) {
        assert!(!self.paused, "Contract is paused");
        let caller = env::signer_account_id();
        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");
        assert!(!result.is_empty(), "Result cannot be empty");
        assert!(
            result.len() <= MAX_RESULT_LEN,
            "Result too long (max {} bytes)",
            MAX_RESULT_LEN
        );

        match escrow.mode {
            EscrowMode::Competitive => {
                // Competitive: workers submit directly on Open escrows (no claim needed)
                assert!(
                    escrow.status == EscrowStatus::Open,
                    "Competitive: escrow must be Open"
                );
                // Competitive result size cap — limits state bloat from many submissions
                assert!(
                    result.len() <= MAX_COMPETITIVE_RESULT_LEN,
                    "Competitive result too long (max {} bytes)",
                    MAX_COMPETITIVE_RESULT_LEN
                );
                // Optional deadline check
                if let Some(deadline) = escrow.deadline_block {
                    assert!(
                        env::block_height() <= deadline,
                        "Submission deadline passed"
                    );
                }
                // Require anti-spam stake for competitive submissions
                let attached = env::attached_deposit().as_yoctonear();
                assert!(
                    attached >= self.worker_stake_yocto,
                    "Competitive submission requires {} yoctoNEAR stake",
                    self.worker_stake_yocto
                );
                // Idempotent: if this worker already submitted, refund attached deposit and return
                if escrow.submissions.iter().any(|s| s.worker == caller) {
                    let attached = env::attached_deposit();
                    if attached.as_yoctonear() > 0 {
                        Promise::new(caller).transfer(attached);
                    }
                    return;
                }
                // Cap check
                let max = escrow.max_submissions.unwrap_or(u32::MAX);
                assert!(
                    (escrow.submissions.len() as u32) < max,
                    "Max submissions reached"
                );
escrow.submissions.push(Submission {
                    worker: env::signer_account_id(),
                    result: result.clone(),
                    stake: U128(self.worker_stake_yocto),
                    worker_pubkey: None,
                });
                self.escrows.insert(&job_id, &escrow);
                emit_event(
                    "competitive_submission",
                    &serde_json::json!({
                        "job_id": job_id,
                        "worker": caller,
                        "submission_count": escrow.submissions.len(),
                    }),
                );
                return; // Stay Open — wait for designate_winner
            }
            EscrowMode::Standard => {
                // Standard flow unchanged
                // Idempotent guard: if already Verifying with the same worker, this is a
                // sandbox replay or mainnet transaction retry — return early, no-op.
                if escrow.status == EscrowStatus::Verifying
                    && escrow.worker.as_ref() == Some(&caller)
                    && escrow.data_id.is_some()
                {
                    return;
                }

                assert!(escrow.status == EscrowStatus::InProgress, "Not in progress");
                let assigned_worker = escrow.worker.clone().expect("No worker assigned — escrow in InProgress without worker");
                assert_eq!(caller, assigned_worker, "Not the worker");

                escrow.result = Some(result);
            }
        }

        let callback_args = serde_json::to_vec(&serde_json::json!({"job_id": job_id}))
            .expect("callback args serialization failed");

        let _promise = env::promise_yield_create(
            "verification_callback",
            &callback_args,
            GAS_FOR_YIELD_CALLBACK,
            GasWeight(0),
            DATA_ID_REGISTER,
        );

        let data_id_bytes = env::read_register(DATA_ID_REGISTER).expect("data_id register not set — promise_yield_create failed");
        let data_id: CryptoHash = data_id_bytes
            .as_slice()
            .try_into()
            .expect("data_id must be 32 bytes");

        escrow.data_id = Some(data_id);
        escrow.status = EscrowStatus::Verifying;
        self.escrows.insert(&job_id, &escrow);
        self.data_id_index.insert(&hex_encode(data_id.as_ref()), &job_id);

        emit_event(
            "result_submitted",
            &serde_json::json!({
                "job_id": job_id,
                "data_id": hex_encode(data_id.as_ref()),
            }),
        );
    }

    // ========================================
    // 5a. Multi-verifier consensus resume
    // ========================================

    /// Resume verification with multi-verifier consensus signatures.
    /// Callable by anyone — trust is in the signatures, not the caller.
    /// Requires `consensus_threshold` valid ed25519 signatures from the verifier_set.
    pub fn resume_verification_multi(
        &mut self,
        data_id_hex: String,
        signed_verdict: SignedVerdict,
    ) -> bool {
        // Must have verifier_set configured
        assert!(!self.verifier_set.is_empty(), "No verifier set configured");

        // Gas bound: no more signatures than verifiers
        assert!(
            signed_verdict.signatures.len() <= self.verifier_set.len(),
            "Too many signatures: {} > {}",
            signed_verdict.signatures.len(),
            self.verifier_set.len()
        );

        // Build scoped message: signatures bind to (data_id || verdict_json)
        // Prevents replay across escrows
        let scoped_message = format!("{}:{}", data_id_hex, signed_verdict.verdict_json);

        // Verify consensus signatures
        let valid_sigs = self.count_valid_signatures(scoped_message.as_bytes(), &signed_verdict.signatures);
        assert!(
            valid_sigs >= self.consensus_threshold,
            "Insufficient valid signatures: {} < {}",
            valid_sigs,
            self.consensus_threshold
        );

        // Parse verdict for sanity check
        let _verdict: VerifierVerdict = serde_json::from_str(&signed_verdict.verdict_json)
            .unwrap_or_else(|_| panic!("Invalid verdict JSON"));

        // Double-resume guard
        let matching_job = self.data_id_index.get(&data_id_hex);
        let job_id_for_guard = matching_job.clone();
        if let Some(ref jid) = matching_job {
            let escrow = self.escrows.get(jid).expect("escrow vanished during index lookup");
            assert!(!escrow.yield_consumed, "Yield already consumed");
        }

        // Decode data_id
        assert!(data_id_hex.len() == 64, "data_id must be 64 hex chars");
        let data_id_bytes: Vec<u8> = (0..64)
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&data_id_hex[i..i + 2], 16)
                    .unwrap_or_else(|_| panic!("Invalid hex at position {}", i))
            })
            .collect();
        let data_id: [u8; 32] = data_id_bytes.try_into().expect("data_id must be 32 bytes");

        // Resume yield — payload is the verdict JSON (not scoped message)
        // verification_callback reads this payload
        let payload = signed_verdict.verdict_json.as_bytes();
        env::promise_yield_resume(&data_id, payload);

        // Mark consumed
        if let Some(jid) = job_id_for_guard {
            let mut escrow = self.escrows.get(&jid).expect("escrow vanished");
            escrow.yield_consumed = true;
            self.escrows.insert(&jid, &escrow);
        }

        true
    }

    /// Count valid ed25519 signatures from active verifiers.
    /// Returns usize to avoid u8 overflow. Silently skips invalid pubkeys.
    fn count_valid_signatures(
        &self,
        message: &[u8],
        signatures: &[VerifierSignature],
    ) -> u8 {
        let mut seen_indices = std::collections::HashSet::new();
        let mut valid: usize = 0;

        for sig in signatures {
            let idx = sig.verifier_index as usize;

            if seen_indices.contains(&idx) {
                continue;
            }
            seen_indices.insert(idx);

            if idx >= self.verifier_set.len() {
                continue;
            }
            let verifier = &self.verifier_set[idx];
            if !verifier.active {
                continue;
            }

            // Gracefully handle bad pubkey stored in verifier_set
            let pk_bytes = match hex_decode_safe(&verifier.public_key) {
                Some(b) if b.len() == 32 => b,
                _ => continue,
            };
            if sig.signature.len() != 64 {
                continue;
            }

            let pk_array: [u8; 32] = match pk_bytes.try_into() {
                Ok(a) => a,
                _ => continue,
            };
            let sig_array: [u8; 64] = match sig.signature.clone().try_into() {
                Ok(a) => a,
                _ => continue,
            };

            if near_sdk::env::ed25519_verify(&sig_array, message, &pk_array) {
                valid += 1;
            }
        }

        valid as u8
    }

    // ========================================
    // 5b. Yield callback — verification_callback
    // ========================================

    /// Called by NEAR runtime when verifier service calls promise_yield_resume(data_id, payload).
    /// Payload must be JSON: {\"score\": 85, \"passed\": true, \"detail\": \"...\"}
    ///
    /// Validates payload consistency (passed must agree with score >= threshold).
    /// Chains FT transfers through settle_callback for proper error handling.
    #[private]
    pub fn verification_callback(&mut self, job_id: String) {
        // Guard: must be invoked as a promise callback, not directly
        let pcount = env::promise_results_count();
        assert!(pcount > 0, "callback only");

        // Read yield resume payload manually
        let result: Result<Vec<u8>, PromiseError> = match env::promise_results_count() {
            0 => Err(PromiseError::Failed),
            _ => match env::promise_result(0) {
                near_sdk::PromiseResult::Successful(data) => Ok(data),
                near_sdk::PromiseResult::Failed => Err(PromiseError::Failed),
            },
        };

        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");

        // Guard: must still be verifying (prevents stale callbacks)
        if escrow.status != EscrowStatus::Verifying {
            return;
        }

        let (settlement_target, verdict) = match result {
            Ok(data) => {
                let parsed: Result<serde_json::Value, _> = serde_json::from_slice(&data);
                match parsed {
                    Ok(val) => {
                        let score = val["score"].as_u64().unwrap_or(0) as u8;
                        let raw_passed = val["passed"].as_bool().unwrap_or(false);
                        let detail = val["detail"].as_str().unwrap_or("no detail").to_string();

                        // Force consistency: can't claim passed with score below threshold
                        let actually_passed = raw_passed && score >= escrow.score_threshold;

                        let verdict = VerifierVerdict {
                            score,
                            passed: actually_passed,
                            detail: detail.clone(),
                        };

                        let target = if actually_passed {
                            SettlementTarget::Claim
                        } else {
                            SettlementTarget::Refund
                        };

                        emit_event(
                            "verification_result",
                            &serde_json::json!({
                                "job_id": job_id,
                                "score": score,
                                "passed": actually_passed,
                                "detail": detail,
                            }),
                        );

                        (target, Some(verdict))
                    }
                    Err(e) => {
                        // Malformed verdict — verifier sent garbage. Don't punish worker.
                        // Full refund to agent + worker stake refunded. Verifier failed, not the worker.
                        log!("Verifier sent malformed payload: {}", e);
                        if let Some(stake) = escrow.worker_stake {
                            if let Some(ref wpk) = escrow.worker_pubkey {
                                // Internal wallet: credit worker's NEAR balance
                                credit_balance(&mut self.balances, wpk, NEAR_TOKEN_ID, stake.0);
                                log!("Crediting worker stake: {} yoctoNEAR to internal wallet {} (malformed verdict)", stake.0, wpk);
                            } else if let Some(ref worker) = escrow.worker {
                                // Legacy: direct NEAR transfer
                                Promise::new(worker.clone())
                                    .transfer(NearToken::from_yoctonear(stake.0));
                                log!("Refunding worker stake: {} yoctoNEAR to {} (malformed verdict)", stake.0, worker);
                            }
                        }
                        escrow.worker_stake = None;
                        emit_event(
                            "verification_malformed",
                            &serde_json::json!({
                                "job_id": job_id,
                                "error": format!("{}", e),
                                "worker_stake_refunded": true,
                            }),
                        );
                        (SettlementTarget::FullRefund, None)
                    }
                }
            }
            Err(_) => {
                // Timeout — nobody verified, full refund to agent.
                // Worker stake REFUNDED to worker — timeout is verifier's fault, not worker's.
                // Worker already did the work and submitted the result.
                if let Some(stake) = escrow.worker_stake {
                    if let Some(ref wpk) = escrow.worker_pubkey {
                        // Internal wallet: credit worker's NEAR balance
                        credit_balance(&mut self.balances, wpk, NEAR_TOKEN_ID, stake.0);
                        log!("Crediting worker stake: {} yoctoNEAR to internal wallet {} (verification timeout)", stake.0, wpk);
                    } else if let Some(ref worker) = escrow.worker {
                        // Legacy: direct NEAR transfer
                        Promise::new(worker.clone())
                            .transfer(NearToken::from_yoctonear(stake.0));
                        log!("Refunding worker stake: {} yoctoNEAR to {} (verification timeout)", stake.0, worker);
                    }
                }
                escrow.worker_stake = None;
                emit_event(
                    "verification_timeout",
                    &serde_json::json!({
                        "job_id": job_id,
                        "worker_stake_refunded": true,
                    }),
                );
                (SettlementTarget::FullRefund, None)
            }
        };

        escrow.verdict = verdict;
        escrow.settlement_target = Some(settlement_target);
        // Clean up data_id index before clearing
        if let Some(ref did) = escrow.data_id {
            self.data_id_index.remove(&hex_encode(did.as_ref()));
        }
        escrow.data_id = None;
        self.escrows.insert(&job_id, &escrow);

        // Chain FT transfers with settlement callback
        self._settle_escrow(&job_id);
    }

    // ========================================
    // Settlement: FT transfers with callback
    // ========================================

    /// Chains FT transfers and attaches a settle_callback to handle success/failure.
    /// Uses .and() to batch transfers so the callback sees all results.
    /// If all transfers succeed → final status (Claimed/Refunded).
    /// If any transfer fails → SettlementFailed (admin can retry).
    fn _settle_escrow(&mut self, job_id: &str) {
        let job_id_string = job_id.to_string();
        let escrow = self.escrows.get(&job_id_string).expect("Escrow not found for settlement");
        let target = escrow
            .settlement_target
            .clone()
            .expect("No settlement target");
        let token = escrow.token.clone();
        let total = escrow.amount.0;
        let vfee = escrow.verifier_fee.0;

        // For worker_pubkey escrows: credit internal balance instead of FT-transfer to daemon.
        // The daemon (relayer) is escrow.worker but shouldn't receive the payout — the worker owns it.
        // Worker withdraws on their own schedule via withdraw().
        if let Some(ref wpk) = escrow.worker_pubkey {
            match target {
                SettlementTarget::Claim => {
                    let payout = total.saturating_sub(vfee);
                    assert!(payout > 0, "Worker payout is zero");
                    credit_balance(&mut self.balances, wpk, &token.to_string(), payout);
                    if vfee > 0 {
                        // Verifier fee goes to owner via FT transfer (not internal)
                    }
                    let mut transfers = vec![];
                    if vfee > 0 {
                        transfers.push(ft_transfer_promise(&token, self.owner.clone(), vfee));
                    }
                    // Refund worker stake to internal NEAR balance
                    if let Some(stake) = escrow.worker_stake {
                        credit_balance(&mut self.balances, wpk, NEAR_TOKEN_ID, stake.0);
                    }
                    self.escrows.insert(&job_id_string, &escrow);
                    if transfers.is_empty() {
                        // No external transfers needed — settle immediately
                        return self._settle_callback_internal(&job_id_string);
                    }
                    // Still need to FT-transfer verifier fee
                    let settle_args = serde_json::to_vec(&serde_json::json!({"job_id": job_id})).expect("settle args");
                    let settle_cb = Promise::new(env::current_account_id()).function_call(
                        "settle_callback".to_string(),
                        settle_args,
                        NearToken::from_yoctonear(0),
                        GAS_FOR_SETTLE_CALLBACK,
                    );
                    let batch = transfers.into_iter().reduce(|acc, p| acc.and(p)).expect("at least one");
                    let _ = batch.then(settle_cb);
                    return;
                }
                SettlementTarget::Refund => {
                    let refund = total.saturating_sub(vfee);
                    assert!(refund > 0, "Agent refund is zero");
                    // Refund worker stake to internal NEAR balance
                    if let Some(stake) = escrow.worker_stake {
                        credit_balance(&mut self.balances, wpk, NEAR_TOKEN_ID, stake.0);
                    }
                    self.escrows.insert(&job_id_string, &escrow);
                    // FT-transfer refund to agent + verifier fee to owner
                    let mut transfers = vec![ft_transfer_promise(&token, escrow.agent.clone(), refund)];
                    if vfee > 0 {
                        transfers.push(ft_transfer_promise(&token, self.owner.clone(), vfee));
                    }
                    let settle_args = serde_json::to_vec(&serde_json::json!({"job_id": job_id})).expect("settle args");
                    let settle_cb = Promise::new(env::current_account_id()).function_call(
                        "settle_callback".to_string(),
                        settle_args,
                        NearToken::from_yoctonear(0),
                        GAS_FOR_SETTLE_CALLBACK,
                    );
                    let batch = transfers.into_iter().reduce(|acc, p| acc.and(p)).expect("at least one");
                    let _ = batch.then(settle_cb);
                    return;
                }
                SettlementTarget::FullRefund => {
                    assert!(total > 0, "Nothing to refund");
                    // Refund worker stake to internal NEAR balance
                    if let Some(stake) = escrow.worker_stake {
                        credit_balance(&mut self.balances, wpk, NEAR_TOKEN_ID, stake.0);
                    }
                    self.escrows.insert(&job_id_string, &escrow);
                    let transfers = vec![ft_transfer_promise(&token, escrow.agent.clone(), total)];
                    let settle_args = serde_json::to_vec(&serde_json::json!({"job_id": job_id})).expect("settle args");
                    let settle_cb = Promise::new(env::current_account_id()).function_call(
                        "settle_callback".to_string(),
                        settle_args,
                        NearToken::from_yoctonear(0),
                        GAS_FOR_SETTLE_CALLBACK,
                    );
                    let batch = transfers.into_iter().reduce(|acc, p| acc.and(p)).expect("at least one");
                    let _ = batch.then(settle_cb);
                    return;
                }
            }
        }

        // Legacy path: no worker_pubkey — direct FT transfer to escrow.worker (NEAR account)
        let transfers: Vec<Promise> = match target {
            SettlementTarget::Claim => {
                let worker = escrow.worker.clone().expect("No worker for claim");
                let payout = total.saturating_sub(vfee);
                assert!(payout > 0, "Worker payout is zero");

                let mut ps = vec![ft_transfer_promise(&token, worker, payout)];
                if vfee > 0 {
                    ps.push(ft_transfer_promise(&token, self.owner.clone(), vfee));
                }
                ps
            }
            SettlementTarget::Refund => {
                let refund = total.saturating_sub(vfee);
                assert!(refund > 0, "Agent refund is zero");

                let mut ps = vec![ft_transfer_promise(&token, escrow.agent.clone(), refund)];
                if vfee > 0 {
                    ps.push(ft_transfer_promise(&token, self.owner.clone(), vfee));
                }
                ps
            }
            SettlementTarget::FullRefund => {
                assert!(total > 0, "Nothing to refund");
                vec![ft_transfer_promise(&token, escrow.agent.clone(), total)]
            }
        };

        let settle_args = serde_json::to_vec(&serde_json::json!({"job_id": job_id})).expect("settle args serialization failed");
        let settle_cb = Promise::new(env::current_account_id()).function_call(
            "settle_callback".to_string(),
            settle_args,
            NearToken::from_yoctonear(0),
            GAS_FOR_SETTLE_CALLBACK,
        );

        let batch = transfers
            .into_iter()
            .reduce(|acc, p| acc.and(p))
            .expect("At least one transfer required");
        let _ = batch.then(settle_cb);
    }

    /// Internal helper: directly settle escrow when all payouts are internal (no FT transfers).
    fn _settle_callback_internal(&mut self, job_id: &String) {
        let mut escrow = self.escrows.get(job_id).expect("Escrow not found");
        let target = escrow.settlement_target.clone().expect("No settlement target");
        escrow.status = match target {
            SettlementTarget::Claim => EscrowStatus::Claimed,
            SettlementTarget::Refund | SettlementTarget::FullRefund => EscrowStatus::Refunded,
        };
        escrow.settlement_target = None;

        // Refund storage deposit to agent
        Promise::new(escrow.agent.clone())
            .transfer(NearToken::from_yoctonear(self.storage_deposit_yocto));

        escrow.worker_stake = None;
        emit_event(
            "escrow_settled",
            &serde_json::json!({
                "job_id": job_id,
                "status": format!("{:?}", escrow.status),
                "internal_wallet": true,
            }),
        );
        self.escrows.insert(job_id, &escrow);
    }

    /// Callback after FT transfer batch completes.
    /// Manually checks ALL promise results (not just one) to catch any failed transfer.
    /// All succeed → final status (Claimed/Refunded) + storage deposit refund.
    /// Any fail → SettlementFailed (admin retries via retry_settlement).
    #[private]
    pub fn settle_callback(&mut self, job_id: String) {
        // Guard against direct calls — must be invoked as a promise callback
        assert!(
            env::promise_results_count() > 0,
            "settle_callback must be called as a promise callback"
        );

        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");
        let target = escrow
            .settlement_target
            .clone()
            .expect("Settlement target not set — cannot settle");

        // Check ALL promise results — .and() batch creates one result per transfer
        let count = env::promise_results_count();
        let mut all_ok = true;
        for i in 0..count {
            match env::promise_result(i) {
                near_sdk::PromiseResult::Successful(_) => {}
                _ => {
                    all_ok = false;
                    break;
                }
            }
        }

        if all_ok {
            escrow.status = match target {
                SettlementTarget::Claim => EscrowStatus::Claimed,
                SettlementTarget::Refund | SettlementTarget::FullRefund => EscrowStatus::Refunded,
            };
            escrow.settlement_target = None;

            // Refund storage deposit to agent
            Promise::new(escrow.agent.clone())
                .transfer(NearToken::from_yoctonear(self.storage_deposit_yocto));
            log!("Refunding storage deposit: {} yoctoNEAR to agent {}", self.storage_deposit_yocto, escrow.agent);

            // Refund worker stake on successful settlement (worker did their job)
            if let Some(stake) = escrow.worker_stake {
                if let Some(ref wpk) = escrow.worker_pubkey {
                    // Internal wallet: already credited in _settle_escrow, just clear
                    log!("Worker stake already credited to internal wallet {} (settlement)", wpk);
                } else if let Some(ref worker) = escrow.worker {
                    Promise::new(worker.clone()).transfer(NearToken::from_yoctonear(stake.0));
                    log!("Refunding worker stake: {} yoctoNEAR to {} (settlement)", stake.0, worker);
                }
            }
            escrow.worker_stake = None;

            emit_event(
                "escrow_settled",
                &serde_json::json!({
                    "job_id": job_id,
                    "status": format!("{:?}", escrow.status),
                }),
            );
        } else {
            escrow.status = EscrowStatus::SettlementFailed;
            emit_event("settlement_failed", &serde_json::json!({"job_id": job_id}));
        }

        self.escrows.insert(&job_id, &escrow);
    }

    // ========================================
    // Admin: retry failed settlements
    // ========================================

    /// Owner can retry a failed settlement immediately.
    /// Non-owners can retry after the escrow has expired (ensures agent has first
    /// chance to fix settlement issues during the escrow's active window).
    /// Also accepts Verifying with settlement_target set — safety net if
    /// verification_callback partially committed before settle failed.
    pub fn retry_settlement(&mut self, job_id: String) {
        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");
        let valid = escrow.status == EscrowStatus::SettlementFailed
            || (escrow.status == EscrowStatus::Verifying && escrow.settlement_target.is_some());
        assert!(
            valid,
            "Not retryable — must be SettlementFailed or Verifying with target"
        );
        assert!(escrow.settlement_target.is_some(), "No settlement target");

        // Max retries — auto-cancel after threshold
        escrow.retry_count += 1;
        if escrow.retry_count > MAX_SETTLEMENT_RETRIES {
            // Force cancel — FT contract may be permanently broken
            escrow.status = EscrowStatus::Cancelled;
            escrow.settlement_target = None;
            self.escrows.insert(&job_id, &escrow);
            // Refund storage deposit
            Promise::new(escrow.agent.clone())
                .transfer(NearToken::from_yoctonear(self.storage_deposit_yocto));
            emit_event(
                "settlement_max_retries_exceeded",
                &serde_json::json!({"job_id": job_id, "retries": escrow.retry_count}),
            );
            return;
        }
        self.escrows.insert(&job_id, &escrow);

        // Owner can retry immediately; anyone else must wait until escrow expires
        let caller = env::predecessor_account_id();
        if caller != self.owner {
            assert!(
                env::block_timestamp_ms() > escrow.created_at + escrow.timeout_ms,
                "Only owner can retry before expiry"
            );
        }

        emit_event(
            "settlement_retried",
            &serde_json::json!({
                "job_id": job_id,
                "caller": caller.to_string(),
                "retry_count": escrow.retry_count,
            }),
        );
        self._settle_escrow(&job_id);
    }

    // ========================================
    // Competitive: designate winner
    // ========================================

    /// Agent (or verifier) designates the winning submission in competitive mode.
    /// Transitions Open → Verifying with yield for the winning worker's result.
    /// Only callable by the escrow agent.
    /// BLOCKER FIX: Cannot designate winner on expired escrows — prevents front-running refund_expired.
    pub fn designate_winner(&mut self, job_id: String, winner_idx: u32) {
        let caller = env::predecessor_account_id();
        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");

        assert_eq!(escrow.mode, EscrowMode::Competitive, "Not competitive");

        // Idempotent: if already Verifying with this winner_idx, this is a sandbox replay
        // or mainnet transaction retry — return early, no-op.
        if escrow.status == EscrowStatus::Verifying
            && escrow.winner_idx == Some(winner_idx)
            && escrow.data_id.is_some()
        {
            return;
        }

        assert_eq!(escrow.status, EscrowStatus::Open, "Must be Open");
        assert!(
            caller == escrow.agent || caller == self.owner,
            "Only agent or owner can designate winner"
        );

        // Timeout enforcement — cannot designate winner on expired escrows
        let now = env::block_timestamp_ms();
        assert!(
            now <= escrow.created_at + escrow.timeout_ms,
            "Cannot designate winner on expired escrow"
        );

        // Deadline check — agent must designate before submission deadline
        if let Some(deadline) = escrow.deadline_block {
            assert!(
                env::block_height() <= deadline,
                "Cannot designate winner after deadline (block {} > {})",
                env::block_height(),
                deadline
            );
        }

        let idx = winner_idx as usize;
        assert!(
            idx < escrow.submissions.len(),
            "winner_idx out of range: {} >= {}",
            winner_idx,
            escrow.submissions.len()
        );

        let winner = escrow.submissions[idx].clone();
        escrow.worker = Some(winner.worker.clone());
        escrow.result = Some(winner.result);
        escrow.winner_idx = Some(winner_idx);
        escrow.status = EscrowStatus::Verifying;

        // Winner's stake goes into worker_stake for normal settlement flow
        escrow.worker_stake = Some(winner.stake.clone());

        // Refund stakes of all non-winning submissions
        for (i, sub) in escrow.submissions.iter().enumerate() {
            if i != idx {
                Promise::new(sub.worker.clone())
                    .transfer(NearToken::from_yoctonear(sub.stake.0));
            }
        }

        // Create yield for verification (same as standard submit_result)
        let callback_args = serde_json::to_vec(&serde_json::json!({"job_id": job_id}))
            .expect("callback args serialization failed");

        let _promise = env::promise_yield_create(
            "verification_callback",
            &callback_args,
            GAS_FOR_YIELD_CALLBACK,
            GasWeight(0),
            DATA_ID_REGISTER,
        );

        let data_id_bytes = env::read_register(DATA_ID_REGISTER)
            .expect("data_id register not set");
        let data_id: CryptoHash = data_id_bytes
            .as_slice()
            .try_into()
            .expect("data_id must be 32 bytes");

        escrow.data_id = Some(data_id);
        self.escrows.insert(&job_id, &escrow);
        self.data_id_index.insert(&hex_encode(data_id.as_ref()), &job_id);

        emit_event(
            "winner_designated",
            &serde_json::json!({
                "job_id": job_id,
                "winner_idx": winner_idx,
                "winner": winner.worker,
                "data_id": hex_encode(data_id.as_ref()),
            }),
        );
    }

    // ========================================
    // Admin: cleanup completed escrows
    // ========================================

    /// Removes terminal escrows (Cancelled, Claimed, Refunded) from state.
    /// Frees storage. Callable by anyone — removing terminal state is always safe.
    /// Respects max_count to avoid gas limits on large cleanups.
    pub fn cleanup_completed(&mut self, max_count: u32) -> u32 {
        if max_count == 0 {
            return 0;
        }

        let mut to_remove: Vec<String> = Vec::new();
        let max = max_count as usize;
        for (jid, e) in self.escrows.iter() {
            if matches!(e.status, EscrowStatus::Claimed | EscrowStatus::Refunded | EscrowStatus::Cancelled) {
                to_remove.push(jid);
                if to_remove.len() >= max {
                    break;
                }
            }
        }

        let count = to_remove.len() as u32;
        for jid in to_remove {
            self.escrows.remove(&jid);
        }

        emit_event(
            "escrows_cleaned",
            &serde_json::json!({
                "count": count,
            }),
        );

        count
    }

    // ========================================
    // Cancel / Refund
    // ========================================

    /// Agent cancels before funding or before worker claims.
    /// PendingFunding → Cancelled + storage deposit refund (no FT to move).
    /// Open → FullRefund via settlement (funds locked, need FT transfer back).
    pub fn cancel(&mut self, job_id: String) {
        let caller = env::signer_account_id();
        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");
        assert_eq!(caller, escrow.agent, "Only agent");

        match escrow.status {
            EscrowStatus::PendingFunding => {
                escrow.status = EscrowStatus::Cancelled;
                self.escrows.insert(&job_id, &escrow);
                // Refund storage deposit
                Promise::new(escrow.agent.clone())
                    .transfer(NearToken::from_yoctonear(self.storage_deposit_yocto));
                log!("Refunding storage deposit on cancel: {} yoctoNEAR to {}", self.storage_deposit_yocto, escrow.agent);
                emit_event("escrow_cancelled", &serde_json::json!({"job_id": job_id}));
            }
            EscrowStatus::Open => {
                escrow.settlement_target = Some(SettlementTarget::FullRefund);
                self.escrows.insert(&job_id, &escrow);
                emit_event(
                    "escrow_cancelled",
                    &serde_json::json!({
                        "job_id": job_id,
                        "reason": "agent_cancelled_funded",
                    }),
                );
                self._settle_escrow(&job_id);
            }
            _ => panic!("Cannot cancel in current state"),
        }
    }

    /// Owner-only emergency recovery for escrows stuck in Verifying state.
    /// Requires VERIFICATION_SAFETY_TIMEOUT_MS (24h) to have elapsed since
    /// the escrow entered Verifying status. This prevents premature cancellation
    /// while the verifier service is still working.
    /// Transitions Verifying → Cancelled, refunds agent via FT, refunds worker stake.
    /// Clears data_id index and yield_consumed flag.
    pub fn force_cancel_verifying(&mut self, job_id: String) {
        assert_eq!(
            env::predecessor_account_id(),
            self.owner,
            "Only owner can force cancel verifying escrows"
        );

        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");
        assert_eq!(
            escrow.status,
            EscrowStatus::Verifying,
            "Escrow is not in Verifying state"
        );

        // Safety timeout: must be at least VERIFICATION_SAFETY_TIMEOUT_MS since creation
        let now = env::block_timestamp_ms();
        let safety_deadline = escrow.created_at + escrow.timeout_ms + VERIFICATION_SAFETY_TIMEOUT_MS;
        assert!(
            now >= safety_deadline,
            "Too early to force cancel — safety timeout not met ({}ms remaining)",
            safety_deadline.saturating_sub(now)
        );

        // Refund worker stake — worker submitted in good faith, verification failed them
        if let Some(stake) = escrow.worker_stake {
            if let Some(ref worker) = escrow.worker {
                Promise::new(worker.clone()).transfer(NearToken::from_yoctonear(stake.0));
                log!(
                    "Force cancel: refunding worker stake {} yoctoNEAR to {}",
                    stake.0,
                    worker
                );
            }
        }
        escrow.worker_stake = None;

        // Clean up data_id index entry
        if let Some(ref did) = escrow.data_id {
            self.data_id_index.remove(&hex_encode(did.as_ref()));
        }
        escrow.data_id = None;
        escrow.yield_consumed = false;

        // Transition to Cancelled and refund escrow amount to agent via FT
        escrow.settlement_target = Some(SettlementTarget::FullRefund);
        escrow.status = EscrowStatus::Cancelled;
        self.escrows.insert(&job_id, &escrow);

        // Refund storage deposit to agent
        Promise::new(escrow.agent.clone())
            .transfer(NearToken::from_yoctonear(self.storage_deposit_yocto));
        log!(
            "Force cancel: refunding storage deposit {} yoctoNEAR to agent {}",
            self.storage_deposit_yocto,
            escrow.agent
        );

        // Execute FT refund for escrow amount
        self._settle_escrow(&job_id);

        emit_event(
            "force_cancel_verifying",
            &serde_json::json!({
                "job_id": job_id,
            }),
        );
    }

    /// Anyone can refund an expired escrow.
    /// PendingFunding → Cancelled + storage refund (no FT).
    /// Open / InProgress → FullRefund via settlement.
    /// Verifying → REJECTED — yield timeout handles this.
    pub fn refund_expired(&mut self, job_id: String) {
        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");
        let now = env::block_timestamp_ms();
        assert!(now > escrow.created_at + escrow.timeout_ms, "Not expired");

        match escrow.status {
            EscrowStatus::PendingFunding => {
                escrow.status = EscrowStatus::Cancelled;
                self.escrows.insert(&job_id, &escrow);
                Promise::new(escrow.agent.clone())
                    .transfer(NearToken::from_yoctonear(self.storage_deposit_yocto));
                log!("Refunding storage deposit on expiry: {} yoctoNEAR to {}", self.storage_deposit_yocto, escrow.agent);
                emit_event(
                    "escrow_cancelled",
                    &serde_json::json!({
                        "job_id": job_id,
                        "reason": "expired_unfunded",
                    }),
                );
            }
            EscrowStatus::Open | EscrowStatus::InProgress => {
                // Forfeit worker stake to agent if InProgress (worker claimed but timed out)
                if escrow.status == EscrowStatus::InProgress {
                    if let Some(stake) = escrow.worker_stake {
                        Promise::new(escrow.agent.clone())
                            .transfer(NearToken::from_yoctonear(stake.0));
                        log!("Forfeiting worker stake: {} yoctoNEAR to agent {} (expired InProgress)", stake.0, escrow.agent);
                    }
                    escrow.worker_stake = None;
                }
                escrow.settlement_target = Some(SettlementTarget::FullRefund);
                self.escrows.insert(&job_id, &escrow);
                emit_event(
                    "escrow_refunded_expired",
                    &serde_json::json!({
                        "job_id": job_id,
                        "from_status": format!("{:?}", escrow.status),
                        "reason": "expired",
                    }),
                );
                self._settle_escrow(&job_id);
            }
            EscrowStatus::Verifying => {
                // Recovery path: allow refund of stuck Verifying escrows after the
                // verification safety timeout has elapsed. This prevents escrows from
                // being permanently stuck if the verifier service goes down AND the
                // yield timeout callback never fires.
                let safety_deadline = escrow.created_at + escrow.timeout_ms + VERIFICATION_SAFETY_TIMEOUT_MS;
                assert!(
                    now >= safety_deadline,
                    "Cannot refund while verifying — safety timeout not met ({}ms remaining). Use force_cancel_verifying after timeout.",
                    safety_deadline.saturating_sub(now)
                );

                // Refund worker stake — worker submitted in good faith, verification stalled
                if let Some(stake) = escrow.worker_stake {
                    if let Some(ref wpk) = escrow.worker_pubkey {
                        credit_balance(&mut self.balances, wpk, NEAR_TOKEN_ID, stake.0);
                        log!("Crediting worker stake: {} yoctoNEAR to internal wallet {} (expired Verifying recovery)", stake.0, wpk);
                    } else if let Some(ref worker) = escrow.worker {
                        Promise::new(worker.clone())
                            .transfer(NearToken::from_yoctonear(stake.0));
                        log!("Refunding worker stake: {} yoctoNEAR to {} (expired Verifying recovery)", stake.0, worker);
                    }
                }
                escrow.worker_stake = None;

                // Clean up data_id index entry
                if let Some(ref did) = escrow.data_id {
                    self.data_id_index.remove(&hex_encode(did.as_ref()));
                }
                escrow.data_id = None;
                escrow.yield_consumed = false;

                escrow.settlement_target = Some(SettlementTarget::FullRefund);
                self.escrows.insert(&job_id, &escrow);
                emit_event(
                    "escrow_refunded_expired",
                    &serde_json::json!({
                        "job_id": job_id,
                        "from_status": "Verifying",
                        "reason": "verification_safety_timeout_expired",
                    }),
                );
                self._settle_escrow(&job_id);
            }
            EscrowStatus::SettlementFailed => {
                // Settlement previously failed — retry it now that time has passed.
                // FT contract may have recovered. Anyone can trigger this on expired escrows.
                self._settle_escrow(&job_id);
            }
            _ => panic!("Already settled"),
        }
    }

    // ========================================
    // Views — paginated, no data_id exposed
    // ========================================

    pub fn get_escrow(&self, job_id: String) -> Option<EscrowView> {
        self.escrows.get(&job_id).map(|e| e.into())
    }

    /// Paginated list of open escrows. Skips `from_index` matching entries.
    /// NOTE: O(n) scan over all escrows. Safe for <10K escrows with pagination caps.
    /// For higher scale, use an offchain indexer.
    pub fn list_open(&self, from_index: Option<u64>, limit: Option<u64>) -> Vec<EscrowView> {
        let from = from_index.unwrap_or(0);
        let max = limit.unwrap_or(50).min(100);
        self.escrows
            .iter()
            .filter(|(_, e)| e.status == EscrowStatus::Open)
            .skip(from as usize)
            .take(max as usize)
            .map(|(_, e)| e.into())
            .collect()
    }

    /// Paginated list of escrows by agent. Skips `from_index` matching entries.
    /// NOTE: O(n) scan over all escrows. Safe for <10K escrows with pagination caps.
    /// For higher scale, use an offchain indexer.
    pub fn list_by_agent(
        &self,
        agent: AccountId,
        from_index: Option<u64>,
        limit: Option<u64>,
    ) -> Vec<EscrowView> {
        let from = from_index.unwrap_or(0);
        let max = limit.unwrap_or(50).min(100);
        self.escrows
            .iter()
            .filter(|(_, e)| e.agent == agent)
            .skip(from as usize)
            .take(max as usize)
            .map(|(_, e)| e.into())
            .collect()
    }

    /// Paginated list of escrows by worker. Skips `from_index` matching entries.
    /// NOTE: O(n) scan over all escrows. Safe for <10K escrows with pagination caps.
    /// For higher scale, use an offchain indexer.
    pub fn list_by_worker(
        &self,
        worker: AccountId,
        from_index: Option<u64>,
        limit: Option<u64>,
    ) -> Vec<EscrowView> {
        let from = from_index.unwrap_or(0);
        let max = limit.unwrap_or(50).min(100);
        self.escrows
            .iter()
            .filter(|(_, e)| e.worker.as_ref() == Some(&worker))
            .skip(from as usize)
            .take(max as usize)
            .map(|(_, e)| e.into())
            .collect()
    }

    /// List escrows by status.
    /// Note: near-sdk 5.x prohibits predecessor checks in view methods.
    /// Callers should verify authorization client-side or use a separate indexing service.
    pub fn list_by_status(
        &self,
        status: String,
        from_index: Option<u64>,
        limit: Option<u64>,
    ) -> Vec<EscrowView> {
        let from = from_index.unwrap_or(0);
        let max = limit.unwrap_or(50).min(100);
        let target: EscrowStatus = match status.as_str() {
            "PendingFunding" => EscrowStatus::PendingFunding,
            "Open" => EscrowStatus::Open,
            "InProgress" => EscrowStatus::InProgress,
            "Verifying" => EscrowStatus::Verifying,
            "Claimed" => EscrowStatus::Claimed,
            "Refunded" => EscrowStatus::Refunded,
            "Cancelled" => EscrowStatus::Cancelled,
            "SettlementFailed" => EscrowStatus::SettlementFailed,
            _ => panic!("Unknown status: {}", status),
        };
        self.escrows
            .iter()
            .filter(|(_, e)| e.status == target)
            .skip(from as usize)
            .take(max as usize)
            .map(|(_, e)| e.into())
            .collect()
    }

    /// Paginated list of escrows in Verifying state — returns only minimal info for verifier routing.
    /// BLOCKER FIX: Only job_id, data_id, and status are returned. No task content,
    /// no worker submission content, no criteria — prevents pre-verification data leaks.
    /// The verifier service already has job details from the escrow_created/result_submitted events.
    /// NOTE: O(n) scan over all escrows. Safe for <10K escrows with pagination caps.
    /// For higher scale, use an offchain indexer.
    pub fn list_verifying(&self, from_index: Option<u64>, limit: Option<u64>) -> Vec<serde_json::Value> {
        let from = from_index.unwrap_or(0);
        let max = limit.unwrap_or(50).min(100);
        self.escrows
            .iter()
            .filter(|(_, e)| e.status == EscrowStatus::Verifying)
            .skip(from as usize)
            .take(max as usize)
            .map(|(_, e)| {
                serde_json::json!({
                    "job_id": e.job_id,
                    "data_id": e.data_id.map(|id| hex_encode(id.as_ref())),
                    "status": "Verifying",
                    // All other fields redacted — use get_escrow for full details
                })
            })
            .collect()
    }

    /// Returns aggregate escrow counts by status.
    /// O(1) — returns cached counters updated on every state transition.
    pub fn get_stats(&self) -> serde_json::Value {
        let s = &self.stats;
        let mut by_status = std::collections::HashMap::new();
        if s.pending_funding > 0 { by_status.insert("PendingFunding".to_string(), s.pending_funding); }
        if s.open > 0 { by_status.insert("Open".to_string(), s.open); }
        if s.in_progress > 0 { by_status.insert("InProgress".to_string(), s.in_progress); }
        if s.verifying > 0 { by_status.insert("Verifying".to_string(), s.verifying); }
        if s.claimed > 0 { by_status.insert("Claimed".to_string(), s.claimed); }
        if s.refunded > 0 { by_status.insert("Refunded".to_string(), s.refunded); }
        if s.cancelled > 0 { by_status.insert("Cancelled".to_string(), s.cancelled); }
        if s.settlement_failed > 0 { by_status.insert("SettlementFailed".to_string(), s.settlement_failed); }
        serde_json::json!({
            "total": s.total(),
            "by_status": by_status,
        })
    }

    /// Returns the contract owner account ID.
    pub fn get_owner(&self) -> AccountId {
        self.owner.clone()
    }

    /// Returns the required storage deposit in yoctoNEAR.
    pub fn get_storage_deposit(&self) -> U128 {
        U128(self.storage_deposit_yocto)
    }

    // ========================================
    // Worker wallet views
    // ========================================

    /// Get a worker's internal balance for a specific token.
    /// Returns 0 if worker or balance not found (not an error).
    pub fn get_worker_balance(&self, worker_pubkey: String, token: Option<String>) -> U128 {
        let token_str = match token {
            Some(t) => t,
            None => NEAR_TOKEN_ID.to_string(),
        };
        let key = balance_key(&worker_pubkey, &token_str);
        self.balances.get(&key).unwrap_or(U128(0))
    }

    /// Get all balances for a worker across all tokens.
    /// Returns array of {token, amount} objects.
    pub fn get_worker_balances(&self, worker_pubkey: String) -> Vec<serde_json::Value> {
        let prefix = format!("{}:", worker_pubkey);
        self.balances
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(k, v)| {
                let token = k.split(':').nth(1).unwrap_or("unknown").to_string();
                serde_json::json!({ "token": token, "amount": v.0.to_string() })
            })
            .collect()
    }

    /// Get worker account info (linked NEAR account, nonce).
    pub fn get_worker_info(&self, worker_pubkey: String) -> Option<WorkerAccountView> {
        self.workers.get(&worker_pubkey).map(|w| w.into())
    }

    /// Check if a worker is paused.
    pub fn is_worker_paused(&self, worker_pubkey: String) -> bool {
        self.paused_workers.get(&worker_pubkey).is_some()
    }

    // ========================================
    // Owner: admin — pause/unpause workers
    // ========================================

    /// Pause a worker — prevents claiming, submitting, and withdrawing.
    /// Owner-only. Worker's balances are preserved (can unpause later).
    pub fn pause_worker(&mut self, worker_pubkey: String) {
        assert_eq!(env::predecessor_account_id(), self.owner, "Only owner");
        assert!(
            self.workers.get(&worker_pubkey).is_some(),
            "Worker not registered"
        );
        self.paused_workers.insert(&worker_pubkey, &true);
        emit_event(
            "worker_paused",
            &serde_json::json!({ "worker_pubkey": worker_pubkey }),
        );
    }

    /// Unpause a previously paused worker.
    /// Owner-only.
    pub fn unpause_worker(&mut self, worker_pubkey: String) {
        assert_eq!(env::predecessor_account_id(), self.owner, "Only owner");
        self.paused_workers.remove(&worker_pubkey);
        emit_event(
            "worker_unpaused",
            &serde_json::json!({ "worker_pubkey": worker_pubkey }),
        );
    }

    // ========================================
    // Owner: configurable parameters
    // ========================================

    /// Update the storage deposit required per escrow. Owner-only.
    pub fn set_storage_deposit(&mut self, amount: U128) {
        assert_eq!(env::predecessor_account_id(), self.owner, "Only owner");
        assert!(amount.0 > 0, "Deposit must be > 0");
        self.storage_deposit_yocto = amount.0;
        log!("Storage deposit updated to {} yoctoNEAR", amount.0);
    }

    /// Update the worker anti-spam stake. Owner-only.
    pub fn set_worker_stake(&mut self, amount: U128) {
        assert_eq!(env::predecessor_account_id(), self.owner, "Only owner");
        self.worker_stake_yocto = amount.0;
        log!("Worker stake updated to {} yoctoNEAR", amount.0);
    }

    /// Returns the current worker stake in yoctoNEAR.
    pub fn get_worker_stake(&self) -> U128 {
        U128(self.worker_stake_yocto)
    }

    /// Owner-only migration. Called when upgrading contract code.
    /// State must already exist — panics on fresh deployments.
    /// Add field migrations here as needed for future versions.
    #[private]
    pub fn migrate() {
        assert!(env::state_exists(), "No state to migrate — use new() for fresh deploy");
        log!("Migration complete — no state changes needed");
    }

    // ── Debug methods for sandbox testing ──────────────────────────────

    /// Get contract's own account ID. Debug/sandbox only.
    pub fn debug_current_account_id(&self) -> String {
        env::current_account_id().to_string()
    }

    /// Verify an ed25519 signature (bytes). Debug/sandbox only.
    pub fn debug_ed25519_verify(
        &self,
        pubkey: Vec<u8>,
        signature: Vec<u8>,
        message: String,
    ) -> bool {
        if pubkey.len() != 32 || signature.len() != 64 {
            return false;
        }
        near_sdk::env::ed25519_verify(
            signature.as_slice().try_into().unwrap(),
            message.as_bytes(),
            pubkey.as_slice().try_into().unwrap(),
        )
    }

    /// Verify an ed25519 signature (hex). Debug/sandbox only.
    pub fn debug_ed25519_verify_hex(
        &self,
        pubkey_hex: String,
        signature: Vec<u8>,
        message: String,
    ) -> bool {
        let pk_bytes = match hex::decode(&pubkey_hex) {
            Ok(b) => b,
            Err(_) => return false,
        };
        self.debug_ed25519_verify(pk_bytes, signature, message)
    }
}

#[cfg(test)]
mod tests;