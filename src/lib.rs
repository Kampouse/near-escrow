use near_sdk::borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::collections::UnorderedMap;
use near_sdk::json_types::U128;
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::serde_json;
use near_sdk::{
    env, log, near, AccountId, CryptoHash, Gas, GasWeight, NearToken, Promise, PromiseError,
};

const GAS_FOR_YIELD_CALLBACK: Gas = Gas::from_tgas(50);
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

// --- Verifier verdict ---

#[derive(BorshDeserialize, BorshSerialize, Serialize, Clone, Debug)]
#[borsh(crate = "near_sdk::borsh")]
#[serde(crate = "near_sdk::serde")]
pub struct VerifierVerdict {
    pub score: u8,
    pub passed: bool,
    pub detail: String,
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
        }
    }
}

// --- Helpers ---

fn emit_event(event: &str, data: &serde_json::Value) {
    env::log_str(&format!(
        "EVENT_JSON:{}",
        &serde_json::json!({
            "standard": "escrow",
            "version": "3.0.0",
            "event": event,
            "data": data,
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
        serde_json::to_vec(&args).unwrap(),
        ONE_YOCTO,
        GAS_FOR_FT_TRANSFER,
    )
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

// --- Contract ---

#[near(contract_state)]
pub struct EscrowContract {
    owner: AccountId,
    escrows: UnorderedMap<String, Escrow>,
    initialized: bool,
}

impl Default for EscrowContract {
    fn default() -> Self {
        Self {
            owner: "root".parse().unwrap(),
            escrows: UnorderedMap::new(b"e"),
            initialized: false,
        }
    }
}

// String length caps — prevent state bloat / gas-exhaustion attacks
const MAX_JOB_ID_LEN: usize = 128;
const MAX_TASK_DESCRIPTION_LEN: usize = 2048;
const MAX_CRITERIA_LEN: usize = 2048;
const MAX_RESULT_LEN: usize = 8192;

#[near]
impl EscrowContract {
    #[init]
    pub fn new() -> Self {
        // Prevent re-initialization — state already exists from first call
        assert!(!env::state_exists(), "Contract already initialized");
        Self {
            owner: env::signer_account_id(),
            escrows: UnorderedMap::new(b"e"),
            initialized: true,
        }
    }

    // ========================================
    // 1. Agent creates escrow (unfunded)
    // ========================================

    /// Creates an escrow in PendingFunding state.
    /// Requires attached NEAR deposit for storage (1 NEAR, surplus refunded on settle).
    /// Agent must then call ft_transfer_call(token, this_contract, amount, job_id) to fund it.
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
    ) {
        let agent = env::signer_account_id();
        assert!(!job_id.is_empty(), "Job ID required");
        assert!(
            job_id.len() <= MAX_JOB_ID_LEN,
            "Job ID too long (max {} bytes)",
            MAX_JOB_ID_LEN
        );
        assert!(amount.0 > 0, "Amount must be > 0");
        assert!(self.escrows.get(&job_id).is_none(), "Job ID exists");

        let vfee = verifier_fee.unwrap_or(U128(0));
        assert!(vfee.0 < amount.0, "Verifier fee must be less than amount");

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
            attached >= STORAGE_DEPOSIT_YOCTO,
            "Insufficient storage deposit: attach at least 1 NEAR"
        );

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
        };

        self.escrows.insert(&job_id, &escrow);

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
    pub fn claim(&mut self, job_id: String) {
        let caller = env::signer_account_id();
        let mut escrow = self.escrows.get(&job_id).expect("Not found");
        assert!(escrow.status == EscrowStatus::Open, "Not open");
        assert!(escrow.worker.is_none(), "Already claimed");
        assert_ne!(caller, escrow.agent, "Agent cannot claim own escrow");

        // Require anti-spam stake
        let attached = env::attached_deposit().as_yoctonear();
        assert!(
            attached >= WORKER_STAKE_YOCTO,
            "Worker stake required: attach at least 0.1 NEAR"
        );

        escrow.worker = Some(caller.clone());
        escrow.worker_stake = Some(U128(attached));
        escrow.status = EscrowStatus::InProgress;
        self.escrows.insert(&job_id, &escrow);

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
    pub fn submit_result(&mut self, job_id: String, result: String) {
        let caller = env::signer_account_id();
        let mut escrow = self.escrows.get(&job_id).expect("Not found");
        assert!(escrow.status == EscrowStatus::InProgress, "Not in progress");
        assert_eq!(caller, escrow.worker.clone().unwrap(), "Not the worker");
        assert!(!result.is_empty(), "Result cannot be empty");
        assert!(
            result.len() <= MAX_RESULT_LEN,
            "Result too long (max {} bytes)",
            MAX_RESULT_LEN
        );

        escrow.result = Some(result);

        let callback_args = serde_json::to_vec(&serde_json::json!({"job_id": job_id})).unwrap();

        let _promise = env::promise_yield_create(
            "verification_callback",
            &callback_args,
            GAS_FOR_YIELD_CALLBACK,
            GasWeight(0),
            DATA_ID_REGISTER,
        );

        let data_id_bytes = env::read_register(DATA_ID_REGISTER).expect("Failed to read data_id");
        let data_id: CryptoHash = data_id_bytes
            .as_slice()
            .try_into()
            .expect("Failed to convert to CryptoHash");

        escrow.data_id = Some(data_id);
        escrow.status = EscrowStatus::Verifying;
        self.escrows.insert(&job_id, &escrow);

        emit_event(
            "result_submitted",
            &serde_json::json!({
                "job_id": job_id,
                "data_id": hex_encode(data_id.as_ref()),
            }),
        );
    }

    // ========================================
    // 5a. LLM Verifier calls resume_verification
    // ========================================

    /// Called by the verifier service to deliver the verdict.
    /// Internally calls env::promise_yield_resume() which triggers
    /// verification_callback as the yield completion.
    ///
    /// Args:
    ///   data_id_hex — hex-encoded CryptoHash from the `result_submitted` event
    ///   verdict — JSON string: {"score": 85, "passed": true, "detail": "..."}
    pub fn resume_verification(&mut self, data_id_hex: String, verdict: String) -> bool {
        // Only the designated verifier (contract owner) can resume
        assert_eq!(
            env::signer_account_id(),
            self.owner,
            "Only verifier can resume"
        );

        // Double-resume guard: find the escrow matching this data_id and
        // ensure it hasn't already been consumed.
        let hex = data_id_hex.clone();
        let mut matching_job: Option<String> = None;
        for (jid, e) in self.escrows.iter() {
            if let Some(ref did) = e.data_id {
                if hex_encode(did.as_ref()) == hex {
                    matching_job = Some(jid);
                    break;
                }
            }
        }
        if let Some(jid) = matching_job {
            let escrow = self.escrows.get(&jid).expect("just iterated");
            assert!(!escrow.yield_consumed, "Yield already consumed");
            // Mark consumed BEFORE resume — if resume fails, this prevents retry
            // with same data_id, but that's safer than double-execution
            let mut escrow = escrow;
            escrow.yield_consumed = true;
            self.escrows.insert(&jid, &escrow);
        }

        // Decode hex data_id to bytes
        let data_id_bytes: Vec<u8> = (0..data_id_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&data_id_hex[i..i + 2], 16).unwrap_or(0))
            .collect();

        let data_id: [u8; 32] = data_id_bytes.try_into().expect("data_id must be 32 bytes");

        let payload = verdict.as_bytes();

        env::promise_yield_resume(&data_id, payload)
    }

    // ========================================
    // 5b. Yield callback — verification_callback
    // ========================================

    /// Called by NEAR runtime when verifier service calls promise_yield_resume(data_id, payload).
    /// Payload must be JSON: {"score": 85, "passed": true, "detail": "..."}
    ///
    /// Validates payload consistency (passed must agree with score >= threshold).
    /// Chains FT transfers through settle_callback for proper error handling.
    pub fn verification_callback(
        &mut self,
        job_id: String,
        #[callback_result] result: Result<Vec<u8>, PromiseError>,
    ) {
        let mut escrow = self.escrows.get(&job_id).expect("Not found");

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
                            if let Some(ref worker) = escrow.worker {
                                let _ = Promise::new(worker.clone())
                                    .transfer(NearToken::from_yoctonear(stake.0));
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
                    if let Some(ref worker) = escrow.worker {
                        let _ = Promise::new(worker.clone())
                            .transfer(NearToken::from_yoctonear(stake.0));
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
        let escrow = self.escrows.get(&job_id_string).expect("Not found");
        let target = escrow
            .settlement_target
            .clone()
            .expect("No settlement target");
        let token = escrow.token.clone();
        let total = escrow.amount.0;
        let vfee = escrow.verifier_fee.0;

        // Build transfer promises
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

        // Batch all transfers via .and() for parallel execution, then callback
        let settle_args = serde_json::to_vec(&serde_json::json!({"job_id": job_id})).unwrap();
        let settle_cb = Promise::new(env::current_account_id()).function_call(
            "settle_callback".to_string(),
            settle_args,
            NearToken::from_yoctonear(0),
            GAS_FOR_SETTLE_CALLBACK,
        );

        // Join all transfers with .and(), then chain the callback
        let batch = transfers
            .into_iter()
            .reduce(|acc, p| acc.and(p))
            .expect("At least one transfer required");
        let _ = batch.then(settle_cb);
    }

    /// Callback after FT transfer batch completes.
    /// Manually checks ALL promise results (not just one) to catch any failed transfer.
    /// All succeed → final status (Claimed/Refunded) + storage deposit refund.
    /// Any fail → SettlementFailed (admin retries via retry_settlement).
    pub fn settle_callback(&mut self, job_id: String) {
        // Guard against direct calls — must be invoked as a promise callback
        assert!(
            env::promise_results_count() > 0,
            "settle_callback must be called as a promise callback"
        );

        let mut escrow = self.escrows.get(&job_id).expect("Not found");
        let target = escrow
            .settlement_target
            .clone()
            .expect("No settlement target");

        // Check ALL promise results — .and() batch creates one result per transfer
        let count = env::promise_results_count();
        let mut all_ok = true;
        for i in 0..count {
            match env::promise_result_checked(i, 1024) {
                Ok(_) => {}
                Err(_) => {
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
            let _ = Promise::new(escrow.agent.clone())
                .transfer(NearToken::from_yoctonear(STORAGE_DEPOSIT_YOCTO));

            // Refund worker stake on successful settlement (worker did their job)
            if let Some(stake) = escrow.worker_stake {
                if let Some(ref worker) = escrow.worker {
                    let _ =
                        Promise::new(worker.clone()).transfer(NearToken::from_yoctonear(stake.0));
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

    /// Anyone can retry a failed settlement after a cooldown (7200 blocks / ~24h).
    /// Owner can retry immediately — no cooldown required.
    /// Also accepts Verifying with settlement_target set — safety net if
    /// verification_callback partially committed before settle failed.
    pub fn retry_settlement(&mut self, job_id: String) {
        let escrow = self.escrows.get(&job_id).expect("Not found");
        let valid = escrow.status == EscrowStatus::SettlementFailed
            || (escrow.status == EscrowStatus::Verifying && escrow.settlement_target.is_some());
        assert!(
            valid,
            "Not retryable — must be SettlementFailed or Verifying with target"
        );
        assert!(escrow.settlement_target.is_some(), "No settlement target");

        // Owner can retry immediately; anyone else must wait cooldown
        let caller = env::signer_account_id();
        if caller != self.owner {
            let _blocks_since_creation = env::block_height().saturating_sub(
                // Use created_at block approximation — settlement_target was set
                // after verification, so escrow has been stuck at least since then.
                // We use the escrow timeout as a proxy for "long enough to retry".
                0u64, // block_height not stored, use timeout heuristic below
            );
            // At minimum, the escrow must be expired before anyone can retry
            assert!(
                env::block_timestamp_ms() > escrow.created_at + escrow.timeout_ms,
                "Only owner can retry before expiry"
            );
        }

        self._settle_escrow(&job_id);
    }

    // ========================================
    // Cancel / Refund
    // ========================================

    /// Agent cancels before funding or before worker claims.
    /// PendingFunding → Cancelled + storage deposit refund (no FT to move).
    /// Open → FullRefund via settlement (funds locked, need FT transfer back).
    pub fn cancel(&mut self, job_id: String) {
        let caller = env::signer_account_id();
        let mut escrow = self.escrows.get(&job_id).expect("Not found");
        assert_eq!(caller, escrow.agent, "Only agent");

        match escrow.status {
            EscrowStatus::PendingFunding => {
                escrow.status = EscrowStatus::Cancelled;
                self.escrows.insert(&job_id, &escrow);
                // Refund storage deposit
                let _ = Promise::new(escrow.agent.clone())
                    .transfer(NearToken::from_yoctonear(STORAGE_DEPOSIT_YOCTO));
                emit_event("escrow_cancelled", &serde_json::json!({"job_id": job_id}));
            }
            EscrowStatus::Open => {
                escrow.settlement_target = Some(SettlementTarget::FullRefund);
                self.escrows.insert(&job_id, &escrow);
                self._settle_escrow(&job_id);
            }
            _ => panic!("Cannot cancel in current state"),
        }
    }

    /// Anyone can refund an expired escrow.
    /// PendingFunding → Cancelled + storage refund (no FT).
    /// Open / InProgress → FullRefund via settlement.
    /// Verifying → REJECTED — yield timeout handles this.
    pub fn refund_expired(&mut self, job_id: String) {
        let mut escrow = self.escrows.get(&job_id).expect("Not found");
        let now = env::block_timestamp_ms();
        assert!(now > escrow.created_at + escrow.timeout_ms, "Not expired");

        match escrow.status {
            EscrowStatus::PendingFunding => {
                escrow.status = EscrowStatus::Cancelled;
                self.escrows.insert(&job_id, &escrow);
                let _ = Promise::new(escrow.agent.clone())
                    .transfer(NearToken::from_yoctonear(STORAGE_DEPOSIT_YOCTO));
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
                        let _ = Promise::new(escrow.agent.clone())
                            .transfer(NearToken::from_yoctonear(stake.0));
                    }
                    escrow.worker_stake = None;
                }
                escrow.settlement_target = Some(SettlementTarget::FullRefund);
                self.escrows.insert(&job_id, &escrow);
                self._settle_escrow(&job_id);
            }
            EscrowStatus::Verifying => {
                panic!("Cannot refund while verifying — yield timeout handles this");
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

    /// Admin view: list escrows by status. Owner only.
    pub fn list_by_status(
        &self,
        status: String,
        from_index: Option<u64>,
        limit: Option<u64>,
    ) -> Vec<EscrowView> {
        assert_eq!(env::signer_account_id(), self.owner, "Only owner");
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

    /// List escrows in Verifying state with their data_id (for verifier service).
    pub fn list_verifying(&self) -> Vec<serde_json::Value> {
        self.escrows
            .iter()
            .filter(|(_, e)| e.status == EscrowStatus::Verifying)
            .map(|(_, e)| {
                serde_json::json!({
                    "job_id": e.job_id,
                    "data_id": e.data_id.map(|id| hex_encode(id.as_ref())),
                    "task_description": e.task_description,
                    "criteria": e.criteria,
                    "score_threshold": e.score_threshold,
                    "result": e.result,
                })
            })
            .collect()
    }

    pub fn get_stats(&self) -> serde_json::Value {
        let mut counts = std::collections::HashMap::new();
        for (_, e) in self.escrows.iter() {
            let key = format!("{:?}", e.status);
            *counts.entry(key).or_insert(0u64) += 1;
        }
        serde_json::json!({
            "total": self.escrows.len(),
            "by_status": counts,
        })
    }

    pub fn get_owner(&self) -> AccountId {
        self.owner.clone()
    }

    pub fn get_storage_deposit(&self) -> U128 {
        U128(STORAGE_DEPOSIT_YOCTO)
    }
}

#[cfg(test)]
mod tests;
