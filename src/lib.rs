use near_sdk::borsh::{self, BorshDeserialize, BorshSerialize};
use near_sdk::collections::UnorderedMap;
use near_sdk::json_types::U128;
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::serde_json;
use near_sdk::{env, near, AccountId, CryptoHash, Gas, GasWeight, NearToken, PromiseError};

const GAS_FOR_YIELD_CALLBACK: Gas = Gas::from_tgas(50);
const GAS_FOR_FT_TRANSFER: Gas = Gas::from_tgas(30);
const ONE_YOCTO: NearToken = NearToken::from_yoctonear(1);
const DATA_ID_REGISTER: u64 = 0;

// --- Verification vote ---

#[derive(BorshDeserialize, BorshSerialize, Serialize, Clone, Debug)]
#[borsh(crate = "near_sdk::borsh")]
#[serde(crate = "near_sdk::serde")]
pub struct VerificationVote {
    pub verifier: AccountId,
    pub score: u8,
    pub passed: bool,
    pub timestamp: u64,
}

// --- Escrow status ---

#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[borsh(crate = "near_sdk::borsh")]
#[serde(crate = "near_sdk::serde")]
pub enum EscrowStatus {
    Locked,
    InProgress,
    Verifying,
    Claimed,
    Refunded,
    Cancelled,
    Failed,
}

// --- Escrow record ---

#[derive(BorshDeserialize, BorshSerialize, Serialize, Clone)]
#[borsh(crate = "near_sdk::borsh")]
#[serde(crate = "near_sdk::serde")]
pub struct Escrow {
    pub job_id: String,
    pub agent: AccountId,
    pub worker: AccountId,
    pub amount: U128,
    pub token: AccountId,
    pub created_at: u64,
    pub timeout_ms: u64,
    pub status: EscrowStatus,
    // Heartbeat
    pub heartbeat_interval_ms: u64,
    pub last_heartbeat: u64,
    // Verification
    pub result_hash: Option<String>,
    pub result_proof: Option<String>,
    pub verifier_count: u8,
    pub min_pass_verifiers: u8,
    pub score_threshold: u8,
    pub verification_fee: U128,
    pub verification_criteria: String,
    pub votes: Vec<VerificationVote>,
    // OutLayer fields (Mode 1)
    pub wasm_url: Option<String>,
    pub input: Option<String>,
    pub max_instructions: Option<u64>,
    pub max_memory_mb: Option<u32>,
    // Yield
    pub data_id: Option<CryptoHash>,
}

fn emit_event(event: &str, data: &serde_json::Value) {
    env::log_str(
        &serde_json::json!({
            "standard": "escrow",
            "version": "2.0.0",
            "event": event,
            "data": data,
        })
        .to_string(),
    );
}

fn ft_transfer_promise(token: &AccountId, receiver: AccountId, amount: u128) -> near_sdk::Promise {
    let args = serde_json::json!({
        "receiver_id": receiver,
        "amount": U128(amount),
    });
    near_sdk::Promise::new(token.clone()).function_call(
        "ft_transfer".to_string(),
        serde_json::to_vec(&args).unwrap(),
        ONE_YOCTO,
        GAS_FOR_FT_TRANSFER,
    )
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[near(contract_state)]
pub struct EscrowContract {
    owner: AccountId,
    outlayer_contract: Option<AccountId>,
    escrows: UnorderedMap<String, Escrow>,
    next_id: u64,
}

impl Default for EscrowContract {
    fn default() -> Self {
        Self {
            owner: "root".parse().unwrap(),
            outlayer_contract: None,
            escrows: UnorderedMap::new(b"e"),
            next_id: 0,
        }
    }
}

#[near]
impl EscrowContract {
    #[init]
    pub fn new(outlayer_contract: Option<AccountId>) -> Self {
        Self {
            owner: env::signer_account_id(),
            outlayer_contract,
            escrows: UnorderedMap::new(b"e"),
            next_id: 0,
        }
    }

    // ========================================
    // Create escrow
    // ========================================

    #[payable]
    pub fn create_escrow(
        &mut self,
        job_id: String,
        worker: AccountId,
        amount: U128,
        token: AccountId,
        timeout_hours: u64,
        heartbeat_interval_minutes: Option<u64>,
        result_hash: Option<String>,
        // Verification
        verifier_count: Option<u8>,
        min_pass_verifiers: Option<u8>,
        score_threshold: Option<u8>,
        verification_fee: Option<U128>,
        verification_criteria: Option<String>,
        // OutLayer params (Mode 1)
        wasm_url: Option<String>,
        input: Option<String>,
        max_instructions: Option<u64>,
        max_memory_mb: Option<u32>,
    ) {
        let agent = env::signer_account_id();
        assert!(!job_id.is_empty(), "Job ID cannot be empty");
        assert!(amount.0 > 0, "Amount must be > 0");
        assert!(self.escrows.get(&job_id).is_none(), "Job ID already exists");

        let heartbeat_ms = heartbeat_interval_minutes.unwrap_or(5) * 60_000;
        let v_count = verifier_count.unwrap_or(0);
        let v_min = min_pass_verifiers.unwrap_or(if v_count > 0 { (v_count / 2 + 1) as u8 } else { 0 });
        let v_threshold = score_threshold.unwrap_or(80);

        let escrow = Escrow {
            job_id: job_id.clone(),
            agent: agent.clone(),
            worker: worker.clone(),
            amount,
            token: token.clone(),
            created_at: env::block_timestamp_ms(),
            timeout_ms: timeout_hours * 3_600_000,
            status: EscrowStatus::Locked,
            heartbeat_interval_ms: heartbeat_ms,
            last_heartbeat: 0,
            result_hash,
            result_proof: None,
            verifier_count: v_count,
            min_pass_verifiers: v_min,
            score_threshold: v_threshold,
            verification_fee: verification_fee.unwrap_or(U128(0)),
            verification_criteria: verification_criteria.unwrap_or_default(),
            votes: Vec::new(),
            wasm_url: wasm_url.clone(),
            input: input.clone(),
            max_instructions,
            max_memory_mb,
            data_id: None,
        };

        self.escrows.insert(&job_id, &escrow);
        self.next_id += 1;

        let mode = if wasm_url.is_some() && input.is_some() { "outlayer" } else { "agent_to_agent" };

        emit_event("escrow_created", &serde_json::json!({
            "job_id": job_id,
            "agent": agent,
            "worker": worker,
            "amount": amount.0.to_string(),
            "mode": mode,
            "verifier_count": v_count,
        }));

        // Mode 1: trigger OutLayer execution via yield
        if wasm_url.is_some() && input.is_some() {
            self._start_outlayer_execution(&job_id);
        }
    }

    // ========================================
    // Mode 2: Agent-to-Agent
    // ========================================

    /// Worker accepts the job
    pub fn accept(&mut self, job_id: String) {
        let caller = env::signer_account_id();
        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");
        assert!(escrow.status == EscrowStatus::Locked, "Escrow not in Locked state");

        // Verify caller is designated worker (or anyone if worker is empty)
        if !escrow.worker.to_string().is_empty() {
            assert_eq!(caller, escrow.worker, "Not the designated worker");
        } else {
            // Anyone can accept — set the caller as worker
            escrow.worker = caller.clone();
        }

        escrow.status = EscrowStatus::InProgress;
        escrow.last_heartbeat = env::block_timestamp_ms();
        self.escrows.insert(&job_id, &escrow);

        emit_event("escrow_accepted", &serde_json::json!({
            "job_id": job_id,
            "worker": caller,
        }));
    }

    /// Worker sends heartbeat to keep escrow alive
    pub fn heartbeat(&mut self, job_id: String) {
        let caller = env::signer_account_id();
        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");
        assert!(escrow.status == EscrowStatus::InProgress, "Escrow not in progress");
        assert_eq!(caller, escrow.worker, "Only worker can heartbeat");

        escrow.last_heartbeat = env::block_timestamp_ms();
        self.escrows.insert(&job_id, &escrow);
    }

    /// Worker submits result
    pub fn submit_result(&mut self, job_id: String, result_proof: String) {
        let caller = env::signer_account_id();
        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");
        assert!(escrow.status == EscrowStatus::InProgress, "Escrow not in progress");
        assert_eq!(caller, escrow.worker, "Only worker can submit result");
        assert!(!result_proof.is_empty(), "Result proof cannot be empty");

        // Verify result hash if set (Mode 2 objective verification)
        if let Some(ref expected_hash) = escrow.result_hash {
            let actual_hash = hex_encode(&env::sha256(result_proof.as_bytes()));
            assert_eq!(actual_hash, expected_hash.as_str(), "Result hash mismatch");
        }

        escrow.result_proof = Some(result_proof);

        if escrow.verifier_count == 0 {
            // No verification needed — auto-settle
            escrow.status = EscrowStatus::Claimed;
            let _ = ft_transfer_promise(&escrow.token.clone(), escrow.worker.clone(), escrow.amount.0);
            emit_event("escrow_claimed", &serde_json::json!({
                "job_id": job_id,
                "worker": escrow.worker.clone(),
                "amount": escrow.amount.0.to_string(),
            }));
        } else {
            // Need verifier votes
            escrow.status = EscrowStatus::Verifying;
            emit_event("escrow_verifying", &serde_json::json!({
                "job_id": job_id,
                "verifier_count": escrow.verifier_count,
            }));
        }

        self.escrows.insert(&job_id, &escrow);
    }

    // ========================================
    // Multi-verifier consensus
    // ========================================

    /// Submit a verification vote
    pub fn verify(&mut self, job_id: String, score: u8) {
        let caller = env::signer_account_id();
        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");
        assert!(escrow.status == EscrowStatus::Verifying, "Escrow not in Verifying state");

        // Worker and agent cannot verify their own job
        assert_ne!(caller, escrow.worker, "Worker cannot verify own job");
        assert_ne!(caller, escrow.agent, "Agent cannot verify own job");

        // Check duplicate vote
        for vote in &escrow.votes {
            assert_ne!(vote.verifier, caller, "Already voted");
        }

        let passed = score >= escrow.score_threshold;
        escrow.votes.push(VerificationVote {
            verifier: caller.clone(),
            score,
            passed,
            timestamp: env::block_timestamp_ms(),
        });

        emit_event("vote_cast", &serde_json::json!({
            "job_id": job_id,
            "verifier": caller,
            "score": score,
            "passed": passed,
        }));

        // Check if we have enough votes to settle
        let pass_count = escrow.votes.iter().filter(|v| v.passed).count() as u8;
        let fail_count = escrow.votes.len() as u8 - pass_count;
        let total_votes = escrow.votes.len() as u8;

        if pass_count >= escrow.min_pass_verifiers {
            // Enough pass votes — pay worker
            escrow.status = EscrowStatus::Claimed;
            let _ = ft_transfer_promise(&escrow.token.clone(), escrow.worker.clone(), escrow.amount.0);
            // Pay verification fees to verifiers
            if escrow.verification_fee.0 > 0 {
                for vote in &escrow.votes {
                    let _ = ft_transfer_promise(
                        &escrow.token.clone(),
                        vote.verifier.clone(),
                        escrow.verification_fee.0,
                    );
                }
            }
            emit_event("escrow_claimed", &serde_json::json!({
                "job_id": job_id,
                "worker": escrow.worker.clone(),
                "amount": escrow.amount.0.to_string(),
                "pass_votes": pass_count,
                "total_votes": total_votes,
            }));
        } else if fail_count > escrow.verifier_count - escrow.min_pass_verifiers {
            // Too many fail votes — refund agent
            escrow.status = EscrowStatus::Refunded;
            let _ = ft_transfer_promise(&escrow.token.clone(), escrow.agent.clone(), escrow.amount.0);
            // Still pay honest verifiers
            if escrow.verification_fee.0 > 0 {
                for vote in &escrow.votes {
                    let _ = ft_transfer_promise(
                        &escrow.token.clone(),
                        vote.verifier.clone(),
                        escrow.verification_fee.0,
                    );
                }
            }
            emit_event("escrow_refunded", &serde_json::json!({
                "job_id": job_id,
                "agent": escrow.agent.clone(),
                "amount": escrow.amount.0.to_string(),
                "reason": "verification_failed",
                "fail_votes": fail_count,
                "total_votes": total_votes,
            }));
        }
        // else: still waiting for more votes

        self.escrows.insert(&job_id, &escrow);
    }

    /// Get all votes for a job
    pub fn get_votes(&self, job_id: String) -> Vec<VerificationVote> {
        self.escrows.get(&job_id).map(|e| e.votes).unwrap_or_default()
    }

    // ========================================
    // Cancel / Refund
    // ========================================

    /// Worker cancels in-progress, or agent cancels locked
    pub fn cancel(&mut self, job_id: String) {
        let caller = env::signer_account_id();
        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");

        match escrow.status {
            EscrowStatus::Locked => {
                assert_eq!(caller, escrow.agent, "Only agent can cancel locked escrow");
            }
            EscrowStatus::InProgress => {
                assert_eq!(caller, escrow.worker, "Only worker can cancel in-progress escrow");
            }
            _ => panic!("Cannot cancel in current state"),
        }

        escrow.status = EscrowStatus::Cancelled;
        let _ = ft_transfer_promise(&escrow.token.clone(), escrow.agent.clone(), escrow.amount.0);
        self.escrows.insert(&job_id, &escrow);

        emit_event("escrow_cancelled", &serde_json::json!({
            "job_id": job_id,
            "cancelled_by": caller,
            "amount": escrow.amount.0.to_string(),
        }));
    }

    /// Refund expired escrow (anyone can call)
    pub fn refund(&mut self, job_id: String) {
        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");
        let now = env::block_timestamp_ms();

        let can_refund = match escrow.status {
            EscrowStatus::Locked => now > escrow.created_at + escrow.timeout_ms,
            EscrowStatus::InProgress => {
                let timeout_expired = now > escrow.created_at + escrow.timeout_ms;
                let heartbeat_expired = escrow.heartbeat_interval_ms > 0
                    && escrow.last_heartbeat > 0
                    && now > escrow.last_heartbeat + escrow.heartbeat_interval_ms;
                timeout_expired || heartbeat_expired
            }
            EscrowStatus::Verifying => now > escrow.created_at + escrow.timeout_ms,
            _ => false,
        };

        assert!(can_refund, "Escrow not refundable yet");

        escrow.status = EscrowStatus::Refunded;
        let _ = ft_transfer_promise(&escrow.token.clone(), escrow.agent.clone(), escrow.amount.0);
        self.escrows.insert(&job_id, &escrow);

        emit_event("escrow_refunded", &serde_json::json!({
            "job_id": job_id,
            "agent": escrow.agent.clone(),
            "amount": escrow.amount.0.to_string(),
            "reason": "timeout",
        }));
    }

    // ========================================
    // Mode 1: OutLayer execution with yield
    // ========================================

    fn _start_outlayer_execution(&mut self, job_id: &String) {
        let escrow = self.escrows.get(job_id).expect("Escrow not found");
        let outlayer = self.outlayer_contract.as_ref().expect("OutLayer contract not set");

        let input_b64 = near_sdk::base64::encode(
            escrow.input.as_deref().unwrap_or("").as_bytes(),
        );

        let args = serde_json::json!({
            "source": {"WasmUrl": {"url": escrow.wasm_url, "hash": "0".repeat(64)}},
            "input_data": input_b64,
            "resource_limits": {
                "max_instructions": escrow.max_instructions.unwrap_or(10_000_000_000),
                "max_memory_mb": escrow.max_memory_mb.unwrap_or(256),
                "max_execution_seconds": 120u64,
            },
        });

        // Cross-contract call to OutLayer → callback
        let promise = near_sdk::Promise::new(outlayer.clone())
            .function_call(
                "request_execution".to_string(),
                serde_json::to_vec(&args).unwrap(),
                NearToken::from_yoctonear(1),
                Gas::from_tgas(300),
            )
            .then(
                near_sdk::Promise::new(env::current_account_id())
                    .function_call(
                        "execution_callback".to_string(),
                        serde_json::to_vec(&(job_id.clone(),)).unwrap(),
                        NearToken::from_yoctonear(0),
                        Gas::from_tgas(50),
                    ),
            );

        let _ = promise;
    }

    /// Callback after OutLayer execution
    pub fn execution_callback(
        &mut self,
        job_id: String,
        #[callback_result] result: Result<serde_json::Value, PromiseError>,
    ) {
        let mut escrow = self.escrows.get(&job_id).expect("Escrow not found");

        match result {
            Ok(val) => {
                let success = val.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
                if success {
                    escrow.status = EscrowStatus::Claimed;
                    escrow.result_proof = val.get("tx_hash").and_then(|v| v.as_str()).map(|s| s.to_string());
                    let _ = ft_transfer_promise(
                        &escrow.token.clone(),
                        escrow.worker.clone(),
                        escrow.amount.0,
                    );
                    emit_event("escrow_claimed", &serde_json::json!({
                        "job_id": job_id,
                        "worker": escrow.worker.clone(),
                        "amount": escrow.amount.0.to_string(),
                        "mode": "outlayer",
                    }));
                } else {
                    escrow.status = EscrowStatus::Failed;
                    let _ = ft_transfer_promise(
                        &escrow.token.clone(),
                        escrow.agent.clone(),
                        escrow.amount.0,
                    );
                    emit_event("escrow_failed", &serde_json::json!({
                        "job_id": job_id,
                        "reason": "execution_failed",
                    }));
                }
            }
            Err(_) => {
                escrow.status = EscrowStatus::Failed;
                let _ = ft_transfer_promise(
                    &escrow.token.clone(),
                    escrow.agent.clone(),
                    escrow.amount.0,
                );
                emit_event("escrow_failed", &serde_json::json!({
                    "job_id": job_id,
                    "reason": "execution_error",
                }));
            }
        }

        self.escrows.insert(&job_id, &escrow);
    }

    // ========================================
    // FT receiving
    // ========================================

    pub fn ft_on_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        msg: String,
    ) -> U128 {
        // Accept all FT transfers (used for funding escrows)
        U128(0) // return 0 = we accept all
    }

    // ========================================
    // Views
    // ========================================

    pub fn get_escrow(&self, job_id: String) -> Option<Escrow> {
        self.escrows.get(&job_id)
    }

    pub fn list_escrows_by_agent(&self, agent: AccountId) -> Vec<Escrow> {
        self.escrows
            .iter()
            .filter(|(_, e)| e.agent == agent)
            .map(|(_, e)| e)
            .collect()
    }

    pub fn list_escrows_by_worker(&self, worker: AccountId) -> Vec<Escrow> {
        self.escrows
            .iter()
            .filter(|(_, e)| e.worker == worker)
            .map(|(_, e)| e)
            .collect()
    }

    pub fn list_pending(&self) -> Vec<Escrow> {
        self.escrows
            .iter()
            .filter(|(_, e)| {
                e.status == EscrowStatus::Locked
                    || e.status == EscrowStatus::InProgress
                    || e.status == EscrowStatus::Verifying
            })
            .map(|(_, e)| e)
            .collect()
    }

    pub fn get_stats(&self) -> serde_json::Value {
        let mut claimed = 0u64;
        let mut refunded = 0u64;
        let mut cancelled = 0u64;
        let mut failed = 0u64;
        let mut verifying = 0u64;
        for (_, e) in self.escrows.iter() {
            match e.status {
                EscrowStatus::Claimed => claimed += 1,
                EscrowStatus::Refunded => refunded += 1,
                EscrowStatus::Cancelled => cancelled += 1,
                EscrowStatus::Failed => failed += 1,
                EscrowStatus::Verifying => verifying += 1,
                _ => {}
            }
        }
        serde_json::json!({
            "total": self.escrows.len(),
            "claimed": claimed,
            "refunded": refunded,
            "cancelled": cancelled,
            "failed": failed,
            "verifying": verifying,
        })
    }

    pub fn get_owner(&self) -> AccountId {
        self.owner.clone()
    }

    pub fn get_outlayer_contract(&self) -> Option<AccountId> {
        self.outlayer_contract.clone()
    }
}
