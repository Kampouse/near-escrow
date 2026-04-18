use near_sdk::near;
use near_sdk::AccountId;

#[near(contract_state)]
pub struct Minimal {
    agent_pubkey: Vec<u8>,
    agent_npub: String,
    escrow_contract: AccountId,
    nonce: u64,
    last_action_block: u64,
    owner: AccountId,
}

impl Default for Minimal {
    fn default() -> Self {
        Self {
            agent_pubkey: vec![0u8; 32],
            agent_npub: String::new(),
            escrow_contract: "root".parse().unwrap(),
            nonce: 0,
            last_action_block: 0,
            owner: "root".parse().unwrap(),
        }
    }
}

#[near]
impl Minimal {
    #[init]
    pub fn new() -> Self {
        assert!(!near_sdk::env::state_exists(), "Already initialized");
        Self::default()
    }

    pub fn hello(&self) -> String {
        "hello".to_string()
    }
}
