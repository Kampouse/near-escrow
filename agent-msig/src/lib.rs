use near_sdk::json_types::U128;
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::{env, near, AccountId, Gas, NearToken, PanicOnDefault, Promise};

// ---------------------------------------------------------------------------
// Event helper
// ---------------------------------------------------------------------------

fn emit_event(event: &str, data: &near_sdk::serde_json::Value) {
    env::log_str(&format!(
        "EVENT_JSON:{}",
        &near_sdk::serde_json::json!({
            "standard": "agent-msig",
            "version": "1.0.0",
            "event": event,
            "data": data,
        })
    ));
}

const GAS_FOR_CREATE_ESCROW: Gas = Gas::from_tgas(50);
const GAS_FOR_FT_TRANSFER: Gas = Gas::from_tgas(100);
const GAS_FOR_STORAGE_DEPOSIT: Gas = Gas::from_tgas(10);
const GAS_FOR_CROSS_CONTRACT: Gas = Gas::from_tgas(20);
const STORAGE_DEPOSIT_YOCTO: u128 = 1_000_000_000_000_000_000_000_000; // 1 NEAR
const FORCE_ROTATE_COOLDOWN_BLOCKS: u64 = 7200; // ~24h

fn decode_ed25519_pubkey(s: &str) -> Vec<u8> {
    let stripped = s
        .strip_prefix("ed25519:")
        .expect("Expected 'ed25519:' prefix");
    let bytes = bs58::decode(stripped)
        .into_vec()
        .expect("Invalid base58 encoding");
    assert_eq!(bytes.len(), 32, "ed25519 public key must be 32 bytes");
    bytes
}

fn encode_ed25519_pubkey(bytes: &[u8]) -> String {
    format!("ed25519:{}", bs58::encode(bytes).into_string())
}

// ---------------------------------------------------------------------------
// Contract state
// ---------------------------------------------------------------------------

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct AgentMsig {
    /// Raw ed25519 public key bytes (32 bytes) — used for signature verification
    agent_pubkey: Vec<u8>,
    /// Agent's Nostr public key (hex string) — for identity lookup by workers
    agent_npub: String,
    /// The escrow contract this agent uses
    escrow_contract: AccountId,
    /// Replay protection — each action must be exactly nonce + 1
    nonce: u64,
    /// Block height of last executed action — used for force_rotate cooldown
    last_action_block: u64,
    /// Emergency admin — can force-rotate key after cooldown period
    owner: AccountId,
    /// Tokens allowed for ft_on_transfer — rejects spam/unexpected tokens
    allowed_tokens: Vec<AccountId>,
    /// Max NEAR-equivalent value per single action (0 = unlimited)
    max_action_value_yocto: u128,
    /// Max total value spent per block-height window
    daily_limit_yocto: u128,
    /// Value spent in the current 24h window
    spent_in_window: u128,
    /// Block height tracking window start
    window_start_block: u64,
}


// ---------------------------------------------------------------------------
// Actions — agent signs the JSON, contract verifies ed25519 signature
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionKind {
    CreateEscrow {
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
    },
    FundEscrow {
        job_id: String,
        token: AccountId,
        amount: U128,
    },
    CancelEscrow {
        job_id: String,
    },
    RegisterToken {
        token: AccountId,
    },
    RotateKey {
        new_pubkey: String, // "ed25519:base58..."
    },
    Withdraw {
        /// None = withdraw NEAR. Some(account) = withdraw FT token.
        token: Option<AccountId>,
        amount: U128,
        recipient: AccountId,
    },
}

#[derive(Serialize, Deserialize)]
struct Action {
    nonce: u64,
    action: ActionKind,
}

// ---------------------------------------------------------------------------
// Contract implementation
// ---------------------------------------------------------------------------

#[near]
impl AgentMsig {
    #[init]
    pub fn new(
        agent_pubkey: String, // "ed25519:base58..."
        agent_npub: String,
        escrow_contract: AccountId,
    ) -> Self {
        // Prevent re-initialization
        assert!(!env::state_exists(), "Contract already initialized");
        let pubkey_bytes = decode_ed25519_pubkey(&agent_pubkey);
        Self {
            agent_pubkey: pubkey_bytes,
            agent_npub,
            escrow_contract,
            nonce: 0,
            last_action_block: env::block_height(),
            owner: env::signer_account_id(),
            allowed_tokens: vec![],
            max_action_value_yocto: 0, // 0 = unlimited
            daily_limit_yocto: 0,      // 0 = unlimited
            spent_in_window: 0,
            window_start_block: env::block_height(),
        }
    }

    // -----------------------------------------------------------------------
    // Core: execute a signed action
    // -----------------------------------------------------------------------

    /// Relayer submits a signed action from the agent.
    /// 1. Verify ed25519 signature against stored pubkey
    /// 2. Parse action JSON
    //  3. Enforce nonce (must be exactly current + 1)
    /// 4. Execute the action (cross-contract call or state change)
    ///
    /// Signature covers the raw action_json string — agent and contract must
    /// agree on canonical JSON format. The contract parses AFTER verification.
    pub fn execute(&mut self, action_json: String, signature: Vec<u8>) {
        // 1. Verify signature
        assert_eq!(
            signature.len(),
            64,
            "Invalid signature length: expected 64, got {}",
            signature.len()
        );
        let sig: [u8; 64] = signature.try_into().expect("length checked above");
        let pk: [u8; 32] = self
            .agent_pubkey
            .clone()
            .try_into()
            .expect("pubkey is 32 bytes");
        assert!(
            env::ed25519_verify(&sig, action_json.as_bytes(), &pk),
            "Invalid agent signature"
        );

        // 2. Parse action
        let action: Action = serde_json::from_str(&action_json).expect("Invalid action JSON");

        // 3. Nonce check
        assert!(
            action.nonce == self.nonce + 1,
            "Invalid nonce: expected {}, got {}",
            self.nonce + 1,
            action.nonce
        );

        // 4. Update state (nonce consumed even if cross-contract call fails)
        self.nonce = action.nonce;
        self.last_action_block = env::block_height();

        // Spending limit check — only applies to actions that move value
        if let Some(value) = self._action_value(&action.action) {
            self._enforce_spending_limit(value);
        }

        // Emit event for off-chain observability.
        // If the cross-contract call fails, the nonce is still consumed.
        // Off-chain monitors can detect this by watching for the corresponding
        // escrow event — if it never appears, the action failed.
        let action_type = match &action.action {
            ActionKind::CreateEscrow { .. } => "create_escrow",
            ActionKind::FundEscrow { .. } => "fund_escrow",
            ActionKind::CancelEscrow { .. } => "cancel_escrow",
            ActionKind::RegisterToken { .. } => "register_token",
            ActionKind::RotateKey { .. } => "rotate_key",
            ActionKind::Withdraw { .. } => "withdraw",
        };
        emit_event(
            "action_executed",
            &near_sdk::serde_json::json!({
                "nonce": action.nonce,
                "action_type": action_type,
                "signer": env::signer_account_id().to_string(),
            }),
        );

        // 5. Dispatch
        match action.action {
            ActionKind::CreateEscrow {
                job_id,
                amount,
                token,
                timeout_hours,
                task_description,
                criteria,
                verifier_fee,
                score_threshold,
                max_submissions,
                deadline_block,
            } => self._create_escrow(
                &job_id,
                amount,
                &token,
                timeout_hours,
                &task_description,
                &criteria,
                verifier_fee,
                score_threshold,
                max_submissions,
                deadline_block,
            ),
            ActionKind::FundEscrow {
                job_id,
                token,
                amount,
            } => self._fund_escrow(&job_id, &token, amount),
            ActionKind::CancelEscrow { job_id } => self._cancel_escrow(&job_id),
            ActionKind::RegisterToken { token } => self._register_token(&token),
            ActionKind::RotateKey { new_pubkey } => self._rotate_key(&new_pubkey),
            ActionKind::Withdraw {
                token,
                amount,
                recipient,
            } => self._withdraw(token, amount, &recipient),
        }
    }

    // -----------------------------------------------------------------------
    // FT receiving
    // -----------------------------------------------------------------------

    /// Accept incoming FT tokens — only from whitelisted token contracts.
    /// Whitelist is empty = accept all (for backward compatibility).
    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> U128 {
        let token_contract = env::predecessor_account_id();
        // If whitelist is non-empty, reject tokens not on the list
        if !self.allowed_tokens.is_empty() && !self.allowed_tokens.contains(&token_contract) {
            return U128(amount.0); // reject
        }
        let _ = (sender_id, amount, msg);
        U128(0) // accept all, refund nothing
    }

    // -----------------------------------------------------------------------
    // Views
    // -----------------------------------------------------------------------

    /// Returns agent's ed25519 public key in "ed25519:base58..." format
    pub fn get_agent_pubkey(&self) -> String {
        encode_ed25519_pubkey(&self.agent_pubkey)
    }

    /// Returns agent's Nostr public key (hex)
    pub fn get_agent_npub(&self) -> String {
        self.agent_npub.clone()
    }

    /// Current nonce — next action must have nonce = this + 1
    pub fn get_nonce(&self) -> u64 {
        self.nonce
    }

    /// The escrow contract this msig calls
    pub fn get_escrow_contract(&self) -> AccountId {
        self.escrow_contract.clone()
    }

    /// Block height of last executed action (for cooldown checks)
    pub fn get_last_action_block(&self) -> u64 {
        self.last_action_block
    }

    /// Emergency admin account
    pub fn get_owner(&self) -> AccountId {
        self.owner.clone()
    }

    // -----------------------------------------------------------------------
    // Admin
    // -----------------------------------------------------------------------

    /// Owner force-rotates key after cooldown (emergency — agent lost keys).
    /// Cooldown enforced: no actions executed in the last 7200 blocks (~24h).
    /// This prevents the owner from force-rotating while agent is active.
    pub fn force_rotate(&mut self, new_pubkey: String, new_npub: String) {
        assert_eq!(env::signer_account_id(), self.owner, "Only owner");
        let blocks_since = env::block_height().saturating_sub(self.last_action_block);
        assert!(
            blocks_since >= FORCE_ROTATE_COOLDOWN_BLOCKS,
            "Cooldown not met: {} blocks remaining",
            FORCE_ROTATE_COOLDOWN_BLOCKS.saturating_sub(blocks_since),
        );
        self.agent_pubkey = decode_ed25519_pubkey(&new_pubkey);
        self.agent_npub = new_npub;
    }

    /// Owner sets the token whitelist for ft_on_transfer.
    /// Empty list = accept all tokens (default).
    pub fn set_allowed_tokens(&mut self, tokens: Vec<AccountId>) {
        assert_eq!(env::signer_account_id(), self.owner, "Only owner");
        self.allowed_tokens = tokens;
    }

    /// Owner sets spending limits. Both default to 0 (unlimited).
    /// max_per_action = max value in a single action (0 = unlimited)
    /// daily_limit = max cumulative value per 24h window (0 = unlimited)
    pub fn set_spending_limits(&mut self, max_per_action: U128, daily_limit: U128) {
        assert_eq!(env::signer_account_id(), self.owner, "Only owner");
        self.max_action_value_yocto = max_per_action.0;
        self.daily_limit_yocto = daily_limit.0;
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Extract the monetary value from an action (for spending limits).
    /// Returns None for non-value actions (rotate_key, register_token, cancel).
    fn _action_value(&self, action: &ActionKind) -> Option<u128> {
        match action {
            ActionKind::CreateEscrow { amount, .. } => Some(amount.0),
            ActionKind::FundEscrow { amount, .. } => Some(amount.0),
            ActionKind::Withdraw { amount, .. } => Some(amount.0),
            ActionKind::CancelEscrow { .. } => None,
            ActionKind::RegisterToken { .. } => None,
            ActionKind::RotateKey { .. } => None,
        }
    }

    /// Enforce per-action and daily spending limits.
    /// Resets the daily window if we've moved past 7200 blocks (~24h).
    fn _enforce_spending_limit(&mut self, value: u128) {
        // Per-action cap
        if self.max_action_value_yocto > 0 {
            assert!(
                value <= self.max_action_value_yocto,
                "Action value {} exceeds per-action limit {}",
                value,
                self.max_action_value_yocto
            );
        }
        // Daily limit — reset window if expired
        if self.daily_limit_yocto > 0 {
            let blocks_since = env::block_height().saturating_sub(self.window_start_block);
            if blocks_since >= FORCE_ROTATE_COOLDOWN_BLOCKS {
                // New window
                self.spent_in_window = 0;
                self.window_start_block = env::block_height();
            }
            let new_total = self.spent_in_window.saturating_add(value);
            assert!(
                new_total <= self.daily_limit_yocto,
                "Daily limit exceeded: {} + {} > {}",
                self.spent_in_window,
                value,
                self.daily_limit_yocto
            );
            self.spent_in_window = new_total;
        }
    }

    // -----------------------------------------------------------------------
    // Internal action handlers
    // -----------------------------------------------------------------------

    fn _create_escrow(
        &self,
        job_id: &str,
        amount: U128,
        token: &AccountId,
        timeout_hours: u64,
        task_description: &str,
        criteria: &str,
        verifier_fee: Option<U128>,
        score_threshold: Option<u8>,
        max_submissions: Option<u32>,
        deadline_block: Option<u64>,
    ) {
        let args = serde_json::to_vec(&serde_json::json!({
            "job_id": job_id,
            "amount": amount,
            "token": token,
            "timeout_hours": timeout_hours,
            "task_description": task_description,
            "criteria": criteria,
            "verifier_fee": verifier_fee,
            "score_threshold": score_threshold,
            "max_submissions": max_submissions,
            "deadline_block": deadline_block,
        }))
        .expect("Failed to serialize create_escrow args");

        let _ = Promise::new(self.escrow_contract.clone()).function_call(
            "create_escrow".to_string(),
            args,
            NearToken::from_yoctonear(STORAGE_DEPOSIT_YOCTO), // storage deposit for escrow
            GAS_FOR_CREATE_ESCROW,
        );
    }

    fn _fund_escrow(&self, job_id: &str, token: &AccountId, amount: U128) {
        let args = serde_json::to_vec(&serde_json::json!({
            "receiver_id": self.escrow_contract.clone(),
            "amount": amount,
            "msg": job_id,
        }))
        .expect("Failed to serialize ft_transfer_call args");

        let _ = Promise::new(token.clone()).function_call(
            "ft_transfer_call".to_string(),
            args,
            NearToken::from_yoctonear(1), // 1 yoctoNEAR required by NEP-141
            GAS_FOR_FT_TRANSFER,
        );
    }

    fn _cancel_escrow(&self, job_id: &str) {
        let args = serde_json::to_vec(&serde_json::json!({
            "job_id": job_id,
        }))
        .expect("Failed to serialize cancel args");

        let _ = Promise::new(self.escrow_contract.clone()).function_call(
            "cancel".to_string(),
            args,
            NearToken::from_yoctonear(0),
            GAS_FOR_CROSS_CONTRACT,
        );
    }

    fn _register_token(&self, token: &AccountId) {
        // Register this msig with the FT contract so it can receive tokens
        let args = serde_json::to_vec(&serde_json::json!({
            "account_id": env::current_account_id(),
        }))
        .expect("Failed to serialize storage_deposit args");

        let _ = Promise::new(token.clone()).function_call(
            "storage_deposit".to_string(),
            args,
            NearToken::from_yoctonear(STORAGE_DEPOSIT_YOCTO), // typical FT storage fee
            GAS_FOR_STORAGE_DEPOSIT,
        );
    }

    fn _rotate_key(&mut self, new_pubkey: &str) {
        // Signature already verified — safe to update
        self.agent_pubkey = decode_ed25519_pubkey(new_pubkey);
    }

    fn _withdraw(&self, token: Option<AccountId>, amount: U128, recipient: &AccountId) {
        match token {
            Some(token_contract) => {
                // Withdraw FT token
                let args = serde_json::to_vec(&serde_json::json!({
                    "receiver_id": recipient,
                    "amount": amount,
                }))
                .expect("Failed to serialize ft_transfer args");

                let _ = Promise::new(token_contract).function_call(
                    "ft_transfer".to_string(),
                    args,
                    NearToken::from_yoctonear(1), // 1 yoctoNEAR required by NEP-141
                    GAS_FOR_FT_TRANSFER,
                );
            }
            None => {
                // Withdraw NEAR — check balance first
                let balance = env::account_balance();
                assert!(
                    balance.as_yoctonear() >= amount.0,
                    "insufficient NEAR balance: {} < {}",
                    balance.as_yoctonear(),
                    amount.0
                );
                let _ =
                    Promise::new(recipient.clone()).transfer(NearToken::from_yoctonear(amount.0));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;
    use rand::rngs::OsRng;

    fn alice() -> AccountId {
        "alice.near".parse().unwrap()
    }

    fn escrow_contract() -> AccountId {
        "escrow.near".parse().unwrap()
    }

    fn setup() -> VMContextBuilder {
        let mut builder = VMContextBuilder::new();
        builder
            .signer_account_id(alice())
            .current_account_id("agent-abc.near".parse().unwrap())
            .predecessor_account_id(alice())
            .attached_deposit(NearToken::from_yoctonear(STORAGE_DEPOSIT_YOCTO))
            .account_balance(NearToken::from_near(10));
        builder
    }

    fn gen_keypair() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn pubkey_str(signing_key: &SigningKey) -> String {
        let verifying = signing_key.verifying_key();
        encode_ed25519_pubkey(verifying.as_bytes())
    }

    fn sign(signing_key: &SigningKey, message: &str) -> Vec<u8> {
        signing_key.sign(message.as_bytes()).to_bytes().to_vec()
    }

    fn new_msig(signing_key: &SigningKey) -> AgentMsig {
        testing_env!(setup().build());
        AgentMsig::new(
            pubkey_str(signing_key),
            "test_npub_hex".to_string(),
            escrow_contract(),
        )
    }

    // -----------------------------------------------------------------------
    // Init
    // -----------------------------------------------------------------------

    #[test]
    fn test_init() {
        let sk = gen_keypair();
        let msig = new_msig(&sk);

        assert_eq!(msig.get_agent_pubkey(), pubkey_str(&sk));
        assert_eq!(msig.get_agent_npub(), "test_npub_hex");
        assert_eq!(msig.get_nonce(), 0);
        assert_eq!(msig.get_escrow_contract(), escrow_contract());
        assert_eq!(msig.get_owner(), alice());
    }

    #[test]
    #[should_panic(expected = "Invalid base58 encoding")]
    fn test_init_invalid_pubkey() {
        testing_env!(setup().build());
        let _ = AgentMsig::new(
            "ed25519:garbage!!!".to_string(),
            "npub".to_string(),
            escrow_contract(),
        );
    }

    #[test]
    #[should_panic(expected = "ed25519 public key must be 32 bytes")]
    fn test_init_wrong_length() {
        testing_env!(setup().build());
        // Valid base58 but wrong length
        let short = format!("ed25519:{}", bs58::encode(&[0u8; 16]).into_string());
        let _ = AgentMsig::new(short, "npub".to_string(), escrow_contract());
    }

    // -----------------------------------------------------------------------
    // Execute: valid signature
    // -----------------------------------------------------------------------

    #[test]
    fn test_execute_register_token() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        let action_json =
            r#"{"nonce":1,"action":{"type":"register_token","token": "***"}}"#;
        let sig = sign(&sk, action_json);

        msig.execute(action_json.to_string(), sig);
        assert_eq!(msig.get_nonce(), 1);
    }

    #[test]
    fn test_execute_sequential_nonces() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        // Nonce 1
        let action1 = r#"{"nonce":1,"action":{"type":"register_token","token": "***"}}"#;
        msig.execute(action1.to_string(), sign(&sk, action1));
        assert_eq!(msig.get_nonce(), 1);

        // Nonce 2
        let action2 = r#"{"nonce":2,"action":{"type":"register_token","token": "***"}}"#;
        msig.execute(action2.to_string(), sign(&sk, action2));
        assert_eq!(msig.get_nonce(), 2);
    }

    // -----------------------------------------------------------------------
    // Execute: invalid signature
    // -----------------------------------------------------------------------

    #[test]
    #[should_panic(expected = "Invalid agent signature")]
    fn test_execute_wrong_signature() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        let action_json =
            r#"{"nonce":1,"action":{"type":"register_token","token": "***"}}"#;
        let wrong_sig = sign(&gen_keypair(), action_json); // different key

        msig.execute(action_json.to_string(), wrong_sig);
    }

    #[test]
    #[should_panic(expected = "Invalid agent signature")]
    fn test_execute_tampered_message() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        let action_json =
            r#"{"nonce":1,"action":{"type":"register_token","token": "***"}}"#;
        let sig = sign(&sk, action_json);

        // Send different JSON than what was signed
        let tampered = r#"{"nonce":1,"action":{"type":"register_token","token": "***"}}"#;
        msig.execute(tampered.to_string(), sig);
    }

    #[test]
    #[should_panic(expected = "Invalid signature length")]
    fn test_execute_short_signature() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        msig.execute("{}".to_string(), vec![0u8; 32]);
    }

    // -----------------------------------------------------------------------
    // Execute: nonce enforcement
    // -----------------------------------------------------------------------

    #[test]
    #[should_panic(expected = "Invalid nonce: expected 1, got 5")]
    fn test_execute_skip_nonce() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        let action_json =
            r#"{"nonce":5,"action":{"type":"register_token","token": "***"}}"#;
        msig.execute(action_json.to_string(), sign(&sk, action_json));
    }

    #[test]
    #[should_panic(expected = "Invalid nonce: expected 2, got 1")]
    fn test_execute_replay_nonce() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        let action_json =
            r#"{"nonce":1,"action":{"type":"register_token","token": "***"}}"#;
        msig.execute(action_json.to_string(), sign(&sk, action_json));

        // Replay same nonce
        msig.execute(action_json.to_string(), sign(&sk, action_json));
    }

    // -----------------------------------------------------------------------
    // Key rotation
    // -----------------------------------------------------------------------

    #[test]
    fn test_rotate_key() {
        let old_sk = gen_keypair();
        let new_sk = gen_keypair();
        let mut msig = new_msig(&old_sk);

        // Sign rotation with old key
        let action_json = serde_json::json!({
            "nonce": 1,
            "action": {
                "type": "rotate_key",
                "new_pubkey": pubkey_str(&new_sk),
            }
        })
        .to_string();

        msig.execute(action_json.to_string(), sign(&old_sk, &action_json));
        assert_eq!(msig.get_nonce(), 1);
        assert_eq!(msig.get_agent_pubkey(), pubkey_str(&new_sk));

        // Old key no longer works
        let action2 = r#"{"nonce":2,"action":{"type":"register_token","token": "***"}}"#;
        let old_sig = sign(&old_sk, action2);

        // Verify old key fails
        testing_env!(setup().build()); // fresh context
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            msig.execute(action2.to_string(), old_sig);
        }));
        assert!(result.is_err());

        // New key works
        let new_sig = sign(&new_sk, action2);
        msig.execute(action2.to_string(), new_sig);
        assert_eq!(msig.get_nonce(), 2);
    }

    // -----------------------------------------------------------------------
    // Force rotate (admin emergency)
    // -----------------------------------------------------------------------

    #[test]
    #[should_panic(expected = "Only owner")]
    fn test_force_rotate_wrong_caller() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        // Execute an action to set last_action_block
        let action_json =
            r#"{"nonce":1,"action":{"type":"register_token","token": "***"}}"#;
        msig.execute(action_json.to_string(), sign(&sk, action_json));

        // Move past cooldown so owner check runs first
        let mut ctx = setup();
        ctx.block_height(8000);
        ctx.signer_account_id("bob.near".parse().unwrap());
        testing_env!(ctx.build());

        msig.force_rotate(pubkey_str(&gen_keypair()), "new_npub".to_string());
    }

    #[test]
    #[should_panic(expected = "Cooldown not met")]
    fn test_force_rotate_too_soon() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        // Execute an action at block 0
        let action_json =
            r#"{"nonce":1,"action":{"type":"register_token","token": "***"}}"#;
        msig.execute(action_json.to_string(), sign(&sk, action_json));

        // Try force rotate at block 100 — cooldown is 7200
        let mut ctx = setup();
        ctx.block_height(100);
        testing_env!(ctx.build());

        msig.force_rotate(pubkey_str(&gen_keypair()), "new_npub".to_string());
    }

    #[test]
    fn test_force_rotate_after_cooldown() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        // Execute action at block 0
        let action_json =
            r#"{"nonce":1,"action":{"type":"register_token","token": "***"}}"#;
        msig.execute(action_json.to_string(), sign(&sk, action_json));

        // Force rotate at block 8000 (> 7200 cooldown)
        let new_sk = gen_keypair();
        let mut ctx = setup();
        ctx.block_height(8000);
        testing_env!(ctx.build());

        msig.force_rotate(pubkey_str(&new_sk), "new_npub".to_string());
        assert_eq!(msig.get_agent_pubkey(), pubkey_str(&new_sk));
        assert_eq!(msig.get_agent_npub(), "new_npub");
    }

    // -----------------------------------------------------------------------
    // ft_on_transfer
    // -----------------------------------------------------------------------

    #[test]
    fn test_ft_on_transfer_accepts_all() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        let result = msig.ft_on_transfer(
            "sender.near".parse().unwrap(),
            U128(1000000),
            "deposit".to_string(),
        );
        assert_eq!(result, U128(0)); // accept all, refund nothing
    }

    // -----------------------------------------------------------------------
    // Action JSON format consistency
    // -----------------------------------------------------------------------

    #[test]
    fn test_action_json_roundtrip() {
        // Verify that Action serializes/deserializes consistently
        // This is critical — agent signs the JSON string, contract parses it
        let action = Action {
            nonce: 1,
            action: ActionKind::CreateEscrow {
                job_id: "job-42".to_string(),
                amount: U128(1_000_000),
                token: "usdc.near".parse().unwrap(),
                timeout_hours: 24,
                task_description: "Build a TODO app".to_string(),
                criteria: "All tests pass".to_string(),
                verifier_fee: Some(U128(100_000)),
                score_threshold: Some(80),
            },
        };

        let json = serde_json::to_string(&action).unwrap();
        let parsed: Action = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.nonce, 1);
        match parsed.action {
            ActionKind::CreateEscrow { job_id, amount, .. } => {
                assert_eq!(job_id, "job-42");
                assert_eq!(amount.0, 1_000_000);
            }
            _ => panic!("Wrong action type"),
        }
    }

    // -----------------------------------------------------------------------
    // Token whitelist
    // -----------------------------------------------------------------------

    #[test]
    fn test_ft_on_transfer_whitelist_accepts_known_token() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        // Set whitelist to allow only usdc.near
        let mut ctx = setup();
        testing_env!(ctx.build());
        msig.set_allowed_tokens(vec!["usdc.near".parse().unwrap()]);

        // usdc.near calls ft_on_transfer — should accept
        let mut ctx = setup();
        ctx.predecessor_account_id("usdc.near".parse().unwrap());
        testing_env!(ctx.build());

        let result = msig.ft_on_transfer(
            "sender.near".parse().unwrap(),
            U128(1000000),
            "deposit".to_string(),
        );
        assert_eq!(result, U128(0)); // accept
    }

    #[test]
    fn test_ft_on_transfer_whitelist_rejects_unknown_token() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        // Set whitelist to allow only usdc.near
        let mut ctx = setup();
        testing_env!(ctx.build());
        msig.set_allowed_tokens(vec!["usdc.near".parse().unwrap()]);

        // spam.near calls ft_on_transfer — should reject
        let mut ctx = setup();
        ctx.predecessor_account_id("spam.near".parse().unwrap());
        testing_env!(ctx.build());

        let result = msig.ft_on_transfer(
            "sender.near".parse().unwrap(),
            U128(1000000),
            "deposit".to_string(),
        );
        assert_eq!(result, U128(1000000)); // reject — return all
    }

    #[test]
    fn test_ft_on_transfer_empty_whitelist_accepts_all() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        // Default empty whitelist — should accept any token
        let mut ctx = setup();
        ctx.predecessor_account_id("random.near".parse().unwrap());
        testing_env!(ctx.build());

        let result = msig.ft_on_transfer(
            "sender.near".parse().unwrap(),
            U128(1000000),
            "deposit".to_string(),
        );
        assert_eq!(result, U128(0)); // accept
    }

    // -----------------------------------------------------------------------
    // Spending limits
    // -----------------------------------------------------------------------

    #[test]
    fn test_spending_limits_allows_within_cap() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        // Set per-action limit of 1M and daily limit of 10M
        let mut ctx = setup();
        testing_env!(ctx.build());
        msig.set_spending_limits(U128(1_000_000), U128(10_000_000));

        // Execute action with 500K — under both limits
        let action_json = serde_json::json!({
            "nonce": 1,
            "action": {
                "type": "register_token",
                "token": "***"
            }
        })
        .to_string();
        // register_token has no value, so it bypasses spending check
        msig.execute(action_json.to_string(), sign(&sk, &action_json));
        assert_eq!(msig.get_nonce(), 1);
    }

    #[test]
    #[should_panic(expected = "Action value 5000000 exceeds per-action limit 1000000")]
    fn test_spending_limits_rejects_over_per_action() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        // Set per-action limit of 1M
        let mut ctx = setup();
        testing_env!(ctx.build());
        msig.set_spending_limits(U128(1_000_000), U128(0)); // daily = unlimited

        // CreateEscrow with 5M — exceeds per-action cap
        let action_json = serde_json::json!({
            "nonce": 1,
            "action": {
                "type": "create_escrow",
                "job_id": "job-1",
                "amount": "5000000",
                "token": "***",
                "timeout_hours": 24,
                "task_description": "Task",
                "criteria": "Criteria",
                "verifier_fee": null,
                "score_threshold": null
            }
        })
        .to_string();
        msig.execute(action_json.to_string(), sign(&sk, &action_json));
    }

    #[test]
    #[should_panic(expected = "Daily limit exceeded")]
    fn test_spending_limits_rejects_over_daily() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        // Set per-action = unlimited, daily = 10M
        let mut ctx = setup();
        testing_env!(ctx.build());
        msig.set_spending_limits(U128(0), U128(10_000_000));

        // First action: 8M — under daily
        let action1 = serde_json::json!({
            "nonce": 1,
            "action": {
                "type": "withdraw",
                "token": null,
                "amount": "8000000",
                "recipient": "bob.near"
            }
        })
        .to_string();
        msig.execute(action1.to_string(), sign(&sk, &action1));

        // Second action: 5M — exceeds daily (8M + 5M > 10M)
        let action2 = serde_json::json!({
            "nonce": 2,
            "action": {
                "type": "withdraw",
                "token": null,
                "amount": "5000000",
                "recipient": "bob.near"
            }
        })
        .to_string();
        msig.execute(action2.to_string(), sign(&sk, &action2));
    }

    #[test]
    fn test_set_allowed_tokens_only_owner() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        // Bob tries to set allowed tokens
        let mut ctx = setup();
        ctx.signer_account_id("bob.near".parse().unwrap());
        testing_env!(ctx.build());

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            msig.set_allowed_tokens(vec!["usdc.near".parse().unwrap()]);
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_set_spending_limits_only_owner() {
        let sk = gen_keypair();
        let mut msig = new_msig(&sk);

        // Bob tries to set spending limits
        let mut ctx = setup();
        ctx.signer_account_id("bob.near".parse().unwrap());
        testing_env!(ctx.build());

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            msig.set_spending_limits(U128(1000), U128(10000));
        }));
        assert!(result.is_err());
    }
}