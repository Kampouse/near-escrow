# Agent Multisig — Design Document

## Problem

Agents only have Nostr keys (secp256k1). They need to authorize actions on NEAR (ed25519).
The relayer currently calls `create_escrow` and `ft_transfer_call` on behalf of the agent,
meaning `escrow.agent` = relayer's account. The relayer can fake, censor, or steal.

The msig IS the agent's NEAR wallet. The relayer becomes a dumb pipe.

## Key Management

**Two separate keys, no cross-curve derivation.**

```
Agent's local config:
  nostr_privkey: secp256k1 (Nostr keypair — signs events, discovery)
  near_auth_key: ed25519   (random — signs msig actions, stored in config)
```

No HKDF. No cross-curve math. The agent generates a random ed25519 keypair at setup,
stores it alongside the Nostr key. Both live in the agent's config file.

The binding between the two keys is operational (same config file), not cryptographic.
Workers verify the binding by calling `msig.get_agent_npub()` — set at deploy time.

**Key rotation**: agent signs `RotateKey` action with OLD ed25519 key. Contract updates pubkey.
No cross-curve verification needed. The old key authorizes the new key. If both are
compromised, agent posts a Nostr event and the contract owner can force-rotate.

## Contract Interface

```rust
#[near(contract_state)]
pub struct AgentMsig {
    agent_pubkey: Vec<u8>,              // raw ed25519 public key bytes (32 bytes)
    agent_npub: String,                 // agent's Nostr public key (hex) — view only
    escrow_contract: AccountId,         // the escrow contract to call
    nonce: u64,                         // replay protection — increments on success
    last_action_block: u64,             // block height of last action — for cooldown
    owner: AccountId,                   // emergency admin (can force-rotate key)
}

#[near]
impl AgentMsig {
    // --- Init ---
    #[init]
    pub fn new(agent_pubkey: String, agent_npub: String, escrow_contract: AccountId) -> Self;

    // --- Core ---
    /// Relayer submits a signed action. Contract verifies ed25519 sig, executes.
    pub fn execute(&mut self, action_json: String, signature: Vec<u8>);

    // --- FT receiving ---
    /// Accept all incoming FT tokens.
    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> U128;

    // --- Views ---
    pub fn get_agent_pubkey(&self) -> String;  // "ed25519:base58..."
    pub fn get_agent_npub(&self) -> String;
    pub fn get_nonce(&self) -> u64;
    pub fn get_escrow_contract(&self) -> AccountId;
    pub fn get_last_action_block(&self) -> u64;
    pub fn get_owner(&self) -> AccountId;

    // --- Admin ---
    /// Owner force-rotates key (emergency — agent lost both keys).
    /// Can only be called if nonce hasn't changed in N blocks.
    pub fn force_rotate(&mut self, new_pubkey: String, new_npub: String);
}
```

## Actions

The agent signs JSON, the contract verifies the ed25519 signature against the stored pubkey:

```rust
struct Action {
    nonce: u64,
    action: ActionKind,
}

enum ActionKind {
    CreateEscrow {
        job_id: String,
        amount: U128,
        token: AccountId,
        timeout_hours: u64,
        task_description: String,
        criteria: String,
        verifier_fee: Option<U128>,
        score_threshold: Option<u8>,
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
        new_pubkey: String,  // "ed25519:base58..."
    },
    Withdraw {
        token: Option<AccountId>,  // None = NEAR
        amount: U128,
        recipient: AccountId,
    },
}
```

**Signature flow:**

```
1. Agent builds Action JSON: {"nonce": 5, "action": {"type": "create_escrow", ...}}
2. Agent signs action_json.as_bytes() with ed25519 private key
3. Agent sends (action_json, signature) to relayer (embedded in Nostr event)
4. Relayer calls msig.execute(action_json, signature)
5. Contract: env::ed25519_verify(signature, action_json_bytes, stored_pubkey)
6. Contract: assert action.nonce == self.nonce + 1
7. Contract: self.nonce = action.nonce
8. Contract: execute action + emit "action_executed" event (NEP-297)
```

## FT Token Handling

**Receiving tokens:**

The msig must register with each FT contract before receiving tokens. Two-step:

```
1. Agent signs: {nonce: 1, action: {type: "register_token", token: "usdc.near"}}
2. msig.execute() → calls storage_deposit on usdc.near (pays from msig NEAR balance)
3. Now anyone can ft_transfer to the msig
4. msig.ft_on_transfer() → returns U128(0) (accept all)
```

**Sending tokens (funding escrow):**

```
1. Agent signs: {nonce: N, action: {type: "fund_escrow", job_id: "job-42", token: "usdc.near", amount: 1000000}}
2. msig.execute() → calls ft_transfer_call(escrow_contract, amount, job_id) on usdc.near
3. Escrow's ft_on_transfer sees sender=msig, token=usdc.near, amount matches → Open
4. escrow.agent = msig account ID (e.g., agent-abc.near)
```

**Note on escrow.agent**: the escrow contract sees the msig as the agent. That's correct.
The msig IS the agent. Workers verify identity through `msig.get_agent_npub()`.

## Cross-Contract Call Failures

When the msig calls the escrow contract and the call fails:

- **State is already committed** — `self.nonce` already incremented, can't roll back
- **Funds are NOT lost** — NEAR returns attached deposit on failure (1 NEAR storage deposit comes back)
- **Agent retries with nonce + 1** — the only cost is the relayer's gas
- **No callback needed** — keeps the contract simple

This is acceptable because:
- create_escrow failures are rare (duplicate job_id — agent's bug)
- ft_transfer_call failures are rare (insufficient balance — agent should check first)
- The cost of a retry is just gas, not funds

## Key Rotation

**Normal rotation (agent still has old key):**

```
1. Agent generates new ed25519 keypair
2. Sign rotation with old key
   let action_json = json!({nonce: N, action: {type: "rotate_key", new_pubkey: "ed25519:new_base58..."}});
3. Contract verifies with OLD pubkey, stores new pubkey
4. Agent updates local config with new key
```

**Emergency rotation (agent lost both keys):**

```
1. Agent posts Nostr event: "Lost keys for agent-abc.near, new npub: abc123..., new pubkey: ed25519:..."
2. Contract owner calls force_rotate(new_pubkey, new_npub)
3. Contract enforces cooldown: nonce must be unchanged for 24 hours (prevents owner from
   force-rotating while agent is actively using the msig)
```

## Gas Economics

**v1 — Agent runs own relayer:**

```
Agent funds relayer's NEAR account for gas.
Relayer is the agent's server — no reimbursement needed.
Cost: ~0.003 NEAR per action (typical cross-contract call).
```

**v2 — Public relayer network:**

```
Agent deposits NEAR into msig for gas sponsorship.
msig.execute() calculates gas used, sends NEAR to relayer in callback.
relayer_whitelist: set of approved relayers.
Anti-griefing: max reimbursement per action (e.g., 0.05 NEAR).
```

## Security Model

| Actor | Can | Can't |
|-------|-----|-------|
| Agent | Authorize any action, rotate key, withdraw | Submit directly (no NEAR for gas), double-spend (nonce) |
| Relayer | Submit signed actions, censor, reorder within nonce | Fake actions (no ed25519 privkey), replay (nonce), steal funds (only whitelisted actions) |
| Contract owner | Force-rotate key after cooldown | Execute actions, move funds, change nonce |
| Attacker | Observe actions (public blockchain) | Fake signatures, replay old actions |

**Threat: relayer censorship**

The relayer can refuse to submit actions. Mitigations:
- Agent can use any relayer (if public network)
- Agent can submit directly if they get NEAR for gas
- Emergency: withdraw all funds and move to new msig

**Threat: relayer front-running**

The relayer sees the signed action before submitting. Could front-run on DEX, etc.
Not relevant here — msig actions are escrow-specific, not market trades.

**Threat: agent key compromise**

Attacker gets the ed25519 private key. Can:
- Authorize any action (create escrow, withdraw)
- Rotate key (lock out original agent)

Mitigation:
- Agent detects via Nostr events or monitoring
- Agent calls force_rotate via contract owner (if they have the old Nostr key to prove identity)
- Or agent withdraws everything to a safe address before attacker does

## Agent Lifecycle

```
1. CREATE AGENT
   - Agent software generates Nostr keypair (secp256k1) and auth keypair (ed25519)
   - Deployer creates AgentMsig contract: agent-abc.near
     with ed25519 pubkey + npub + escrow_contract address
   - Deployer funds msig with NEAR (for storage deposits + FT registration)
   - Deployer registers msig with FT contract (storage_deposit on FT contract)

2. POST TASK
   - Agent signs Nostr event (kind 41000) with secp256k1 key
   - Event tags: job_id, reward, escrow_contract
   - Event posted to Nostr relays

3. CREATE ESCROW
   - Agent signs action: {nonce: 1, action: {type: "create_escrow", ...params}}
   - Embeds signed action in Nostr event (kind 41000) tags: action + action_sig
   - Relayer extracts signed action, calls msig.execute(action_json, signature)
   - msig verifies sig, calls escrow.create_escrow(), attaches 1 NEAR storage deposit
   - escrow.agent = agent-abc.near (the msig)

4. FUND ESCROW
   - Agent signs action: {nonce: 2, action: {type: "fund_escrow", job_id, token, amount}}
   - Posts as Nostr kind 41003 event
   - Relayer calls msig.execute()
   - msig calls ft_transfer_call(escrow_contract, amount, job_id) on FT contract
   - Escrow receives tokens → status: Open

5. WORKER CLAIMS → SUBMITS → VERIFIER SCORES → SETTLES
   - Same as current flow. Escrow contract doesn't care that agent is an msig.

6. RECEIVE PAYOUT (if verification fails → refund)
   - Escrow calls ft_transfer(agent-abc.near, refund_amount)
   - msig.ft_on_transfer() accepts tokens back

7. WITHDRAW (if agent wants to move funds out)
   - Agent signs: {nonce: N, action: {type: "withdraw", token: "usdc.near", amount, recipient}}
   - msig calls ft_transfer(recipient, amount) or sends NEAR
```

## Full Trust Chain

```
Agent's Nostr event (signed with secp256k1)
  → Workers discover task on Nostr
  → Workers check: msig.get_agent_npub() matches event's pubkey
  → Trust established: this escrow belongs to that Nostr agent

Agent's signed action (signed with ed25519)
  → Relayer submits to msig
  → msig verifies ed25519 signature
  → Calls escrow contract as the agent
  → On-chain authorization is cryptographic, not trust-based

Nostr event ←→ msig binding
  → Deployer set npub at creation
  → Workers verify off-chain (call msig view + match Nostr event)
  → Not cryptographic between curves, but verifiable via deployer trust
```

## Differences from Current Architecture

| | Current (relayer as agent) | msig |
|---|---|---|
| escrow.agent | relayer's NEAR account | msig contract address |
| Who can create escrow | Anyone (relayer trusted) | Only ed25519 key holder |
| Who can cancel | relayer (escrow.agent) | Agent (via msig) |
| Who can fund | relayer (must match escrow.agent) | Agent (via msig) |
| Relayer trust | Full (can fake/steal) | Minimal (can only censor) |
| Agent key | None (relayer has NEAR key) | ed25519 (agent's config) |
| Gas payer | Relayer | Relayer (v1), msig (v2) |
| Identity | relayer's account name | msig.get_agent_npub() |
