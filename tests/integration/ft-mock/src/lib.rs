use near_sdk::borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::collections::LookupMap;
use near_sdk::json_types::U128;

use near_sdk::{env, near, AccountId, Gas, NearToken, Promise};

const STORAGE_DEPOSIT_YOCTO: u128 = 1_000_000_000_000_000_000_000_000; // 1 NEAR
const GAS_FOR_RESOLVE_TRANSFER: Gas = Gas::from_tgas(10);
const GAS_FOR_FT_ON_TRANSFER: Gas = Gas::from_tgas(10);

#[derive(BorshDeserialize, BorshSerialize)]
#[borsh(crate = "near_sdk::borsh")]
pub struct Account {
    pub balance: u128,
}

#[near(contract_state)]
pub struct FtMock {
    owner: AccountId,
    accounts: LookupMap<AccountId, Account>,
    total_supply: u128,
    /// When true, ft_transfer and ft_transfer_call panic with "Transfers paused".
    /// Used by integration tests to force SettlementFailed in escrow.
    transfers_paused: bool,
}

impl Default for FtMock {
    fn default() -> Self {
        Self {
            owner: "root".parse().unwrap(),
            accounts: LookupMap::new(b"a"),
            total_supply: 0,
            transfers_paused: false,
        }
    }
}

#[near]
impl FtMock {
    #[init]
    pub fn new() -> Self {
        assert!(!env::state_exists(), "Already initialized");
        Self {
            owner: env::signer_account_id(),
            accounts: LookupMap::new(b"a"),
            total_supply: 0,
            transfers_paused: false,
        }
    }

    /// Mint tokens to an account (owner only, for testing)
    pub fn mint(&mut self, account_id: AccountId, amount: U128) {
        assert_eq!(
            env::signer_account_id(),
            self.owner,
            "Only owner can mint"
        );
        let mut acct = self.accounts.get(&account_id).unwrap_or(Account { balance: 0 });
        acct.balance += amount.0;
        self.accounts.insert(&account_id, &acct);
        self.total_supply += amount.0;
    }

    /// Register an account for storage (standard NEP-141 pattern)
    #[payable]
    pub fn storage_deposit(&mut self, account_id: Option<AccountId>) {
        let account_id = account_id.unwrap_or(env::signer_account_id());
        let deposit = env::attached_deposit().as_yoctonear();

        let registered = self.accounts.get(&account_id).is_some();
        if !registered {
            self.accounts
                .insert(&account_id, &Account { balance: 0 });
        }

        // Refund excess deposit
        if deposit > STORAGE_DEPOSIT_YOCTO {
            let refund = deposit - STORAGE_DEPOSIT_YOCTO;
            if !registered {
                let _ = Promise::new(env::signer_account_id())
                    .transfer(NearToken::from_yoctonear(refund));
            }
        }
    }

    /// Check if account is registered
    pub fn storage_balance_of(&self, account_id: AccountId) -> Option<serde_json::Value> {
        if self.accounts.get(&account_id).is_some() {
            Some(serde_json::json!({
                "total": U128(STORAGE_DEPOSIT_YOCTO),
                "available": U128(0)
            }))
        } else {
            None
        }
    }

    /// Standard NEP-141 ft_transfer
    #[payable]
    pub fn ft_transfer(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>) {
        assert!(!self.transfers_paused, "Transfers paused");
        let sender_id = env::predecessor_account_id();
        assert!(
            env::attached_deposit().as_yoctonear() >= 1,
            "Requires 1 yoctoNEAR deposit"
        );
        assert!(amount.0 > 0, "Amount must be positive");

        let mut sender = self
            .accounts
            .get(&sender_id)
            .expect("Sender not registered");
        assert!(sender.balance >= amount.0, "Insufficient balance");
        sender.balance -= amount.0;
        self.accounts.insert(&sender_id, &sender);

        let mut receiver = self
            .accounts
            .get(&receiver_id)
            .expect("Receiver not registered");
        receiver.balance += amount.0;
        self.accounts.insert(&receiver_id, &receiver);

        let _ = memo; // ignore memo
    }

    /// Standard NEP-141 ft_transfer_call
    #[payable]
    pub fn ft_transfer_call(
        &mut self,
        receiver_id: AccountId,
        amount: U128,
        memo: Option<String>,
        msg: String,
    ) -> U128 {
        assert!(!self.transfers_paused, "Transfers paused");
        let sender_id = env::predecessor_account_id();
        assert!(
            env::attached_deposit().as_yoctonear() >= 1,
            "Requires 1 yoctoNEAR deposit"
        );
        assert!(amount.0 > 0, "Amount must be positive");

        // Deduct from sender
        let mut sender = self
            .accounts
            .get(&sender_id)
            .expect("Sender not registered");
        assert!(sender.balance >= amount.0, "Insufficient balance");
        sender.balance -= amount.0;
        self.accounts.insert(&sender_id, &sender);

        // Credit to receiver temporarily
        let mut receiver = self
            .accounts
            .get(&receiver_id)
            .expect("Receiver not registered");
        receiver.balance += amount.0;
        self.accounts.insert(&receiver_id, &receiver);

        let _ = memo;

        // Call receiver's ft_on_transfer
        let args = serde_json::json!({
            "sender_id": sender_id,
            "amount": amount,
            "msg": msg,
        });

        let promise = Promise::new(receiver_id.clone()).function_call(
            "ft_on_transfer".to_string(),
            serde_json::to_vec(&args).unwrap(),
            NearToken::from_yoctonear(0),
            GAS_FOR_FT_ON_TRANSFER,
        );

        // Then resolve the transfer
        let resolve_args = serde_json::json!({
            "sender_id": sender_id,
            "receiver_id": receiver_id,
            "amount": amount,
        });

        let resolve_promise = Promise::new(env::current_account_id()).function_call(
            "ft_resolve_transfer".to_string(),
            serde_json::to_vec(&resolve_args).unwrap(),
            NearToken::from_yoctonear(0),
            GAS_FOR_RESOLVE_TRANSFER,
        );

        let _ = promise.then(resolve_promise);

        // We return U128(1) as a convention; the actual unused amount is handled
        // in ft_resolve_transfer
        U128(1)
    }

    /// Called by the FT contract's own callback to handle refunds
    #[private]
    pub fn ft_resolve_transfer(
        &mut self,
        sender_id: AccountId,
        receiver_id: AccountId,
        amount: U128,
    ) -> U128 {
        // Check the promise result from ft_on_transfer
        let result = env::promise_result(0);
        let unused_amount = match result {
            near_sdk::PromiseResult::Successful(data) => {
                // Parse the returned U128 amount
                let returned: U128 =
                    serde_json::from_slice(&data).unwrap_or(U128(0));
                returned.0
            }
            _ => {
                // ft_on_transfer failed — refund everything
                amount.0
            }
        };

        // Refund unused amount from receiver back to sender
        if unused_amount > 0 {
            let actual_refund = unused_amount.min(amount.0);
            let mut receiver = self.accounts.get(&receiver_id).unwrap();
            if receiver.balance >= actual_refund {
                receiver.balance -= actual_refund;
                self.accounts.insert(&receiver_id, &receiver);

                let mut sender = self.accounts.get(&sender_id).unwrap();
                sender.balance += actual_refund;
                self.accounts.insert(&sender_id, &sender);
            }
        }

        U128(unused_amount)
    }

    /// Standard NEP-141 ft_on_transfer — accept all tokens
    pub fn ft_on_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        msg: String,
    ) -> U128 {
        let _ = (sender_id, amount, msg);
        U128(0) // accept all
    }

    /// Standard NEP-141 ft_balance_of
    pub fn ft_balance_of(&self, account_id: AccountId) -> U128 {
        let account = self.accounts.get(&account_id).unwrap_or(Account { balance: 0 });
        U128(account.balance)
    }

    /// Standard NEP-141 ft_metadata
    pub fn ft_metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "spec": "ft-1.0.0",
            "name": "Mock Token",
            "symbol": "MTK",
            "decimals": 6,
            "icon": null,
        })
    }

    /// Get total supply
    pub fn ft_total_supply(&self) -> U128 {
        U128(self.total_supply)
    }

    // ========================================
    // Test-only: toggle pause on FT transfers
    // ========================================

    /// Pause all ft_transfer / ft_transfer_call calls (owner only).
    /// Causes next settlement attempt to fail → SettlementFailed.
    pub fn pause_transfers(&mut self) {
        assert_eq!(env::signer_account_id(), self.owner, "Only owner");
        self.transfers_paused = true;
    }

    /// Unpause transfers (owner only). After unpausing, retry_settlement succeeds.
    pub fn unpause_transfers(&mut self) {
        assert_eq!(env::signer_account_id(), self.owner, "Only owner");
        self.transfers_paused = false;
    }

    /// Read current pause state (for assertions in tests)
    pub fn is_transfers_paused(&self) -> bool {
        self.transfers_paused
    }
}
