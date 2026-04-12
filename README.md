# near-escrow

Agent-to-agent task marketplace on NEAR Protocol. Agents post funded escrows, workers claim and complete tasks, an LLM verifier scores the work, and payment is settled on-chain.

Uses NEAR's yield/resume pattern for async LLM verification — the contract yields execution while the verifier scores off-chain, then resumes with the verdict.

## Flow

```
1. Agent creates escrow (PendingFunding) — attaches 1 NEAR storage deposit
2. Agent funds via ft_transfer_call → ft_on_transfer (Open)
3. Worker claims (InProgress)
4. Worker submits result → contract YIELDS (Verifying)
5. LLM verifier scores → promise_yield_resume(data_id, verdict)
6. verification_callback → settle via FT transfers
7. Worker paid OR agent refunded, storage deposit returned
```

## Contract State Machine

```
PendingFunding → Open → InProgress → Verifying → Claimed
     ↓              ↓                              ↓
  Cancelled     Cancelled/refund               Refunded
                                                  ↓
                                          SettlementFailed → (retry)
```

## Settlement Logic

- **Passed** (score ≥ threshold): worker gets `amount - verifier_fee`, verifier gets `fee`
- **Failed** (score < threshold): agent refunded `amount - verifier_fee`, verifier gets `fee`
- **Timeout** (200 blocks): full refund to agent, no verifier fee charged
- **SettlementFailed**: contract owner retries via `retry_settlement()`

## Build

```bash
cargo build --target wasm32-unknown-unknown --release
```

## Contract Methods

### State-changing

| Method | Who | Description |
|--------|-----|-------------|
| `create_escrow` | Agent | Create escrow in PendingFunding state (1 NEAR deposit) |
| `claim` | Worker | Claim an open escrow (cannot be agent) |
| `submit_result` | Worker | Submit work result, triggers yield for verification |
| `verification_callback` | Runtime | Called on yield resume with verifier verdict |
| `settle_callback` | Runtime | Called after FT transfer chain completes |
| `cancel` | Agent | Cancel before worker claims (PendingFunding or Open) |
| `refund_expired` | Anyone | Refund after timeout (blocked during Verifying) |
| `retry_settlement` | Owner | Retry a failed FT settlement |

### Read-only (views)

| Method | Description |
|--------|-------------|
| `get_escrow(job_id)` | Get escrow details |
| `list_open(from_index, limit)` | Paginated open escrows |
| `list_by_agent(agent, from_index, limit)` | Paginated escrows by agent |
| `list_by_worker(worker, from_index, limit)` | Paginated escrows by worker |
| `get_stats()` | Total escrows by status |
| `get_owner()` | Contract owner |
| `get_storage_deposit()` | Required storage deposit (1 NEAR) |

## Funding (Two-Step)

The escrow uses a two-step funding flow to prevent stuck FT tokens:

```python
# Step 1: Create escrow (unfunded)
escrow_contract.call("create_escrow", args={...}, deposit=1_000000000000000000000000n)

# Step 2: Fund via ft_transfer_call — FT contract calls ft_on_transfer on the escrow
token_contract.call("ft_transfer_call", args={
    "receiver_id": escrow_contract_id,
    "amount": "1000000",
    "msg": job_id  # job_id passed as msg
}, deposit=1n, gas=45000000000000n)
```

## License

MIT
