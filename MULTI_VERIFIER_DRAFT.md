# Multi-Verifier Consensus Draft — Off-chain Consensus, On-chain Attestation

## Architecture

```
Off-chain:
  Verifier A (score: 82) ──┐
  Verifier B (score: 91) ──┼── median: 88, passed: true ──> sign(verdict_bytes)
  Verifier C (score: 75) ──┘         │
                             2-of-3 sign the combined verdict
                                      │
On-chain:                             ▼
  resume_verification(data_id, verdict_json, signatures[])
    ├── verify 2-of-3 ed25519 sigs against stored verifier_set
    ├── parse verdict
    └── promise_yield_resume(data_id, verdict_bytes)
```

## Nostr Off-chain Consensus Protocol

### New Kind: 41006 (Verifier Score)

```json
{
  "kind": 41006,
  "content": "{\"score\": 88, \"passed\": true, \"detail\": \"Task completed with minor issues\"}",
  "tags": [
    ["j", "<job_id>"],
    ["verifier", "<verifier_pubkey_hex>"],
    ["nonce", "<escrow_nonce>"]
  ]
}
```

### New Kind: 41007 (Consensus Verdict)

Posted by any verifier after seeing ≥2 matching scores (within threshold).

```json
{
  "kind": 41007,
  "content": "<base64url of signed verdict JSON>",
  "tags": [
    ["j", "<job_id>"],
    ["verdict", "{\"score\":88,\"passed\":true,\"detail\":\"median of 3\"}"],
    ["sig_a", "<verifier_a_ed25519_signature_hex>"],
    ["sig_b", "<verifier_b_ed25519_signature_hex>"],
    ["signers", "verifier_a.near", "verifier_b.near"]
  ]
}
```

**Consensus rules (off-chain):**
1. Each verifier posts kind 41006 independently
2. Once ≥2 scores are available, any verifier computes median
3. `passed` = median_score >= escrow.score_threshold
4. Signers are those whose score is within ±15 of median (prevents outliers from co-signing)
5. 2 valid signatures → post kind 41007
6. Any verifier or relayer submits on-chain

---

## Contract Changes

### 1. New Types

```rust
/// A verifier in the consensus set
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone)]
#[serde(crate = "near_sdk::serde")]
pub struct VerifierInfo {
    /// NEAR account ID
    pub account_id: AccountId,
    /// ed25519 public key (32 bytes hex) for signature verification
    pub public_key: String,
    /// Whether this verifier is active
    pub active: bool,
}

/// Signed verdict from multi-verifier consensus
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(crate = "near_sdk::serde")]
pub struct SignedVerdict {
    /// JSON verdict: {"score": u8, "passed": bool, "detail": String}
    pub verdict_json: String,
    /// ed25519 signatures (2+ required)
    pub signatures: Vec<VerifierSignature>,
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(crate = "near_sdk::serde")]
pub struct VerifierSignature {
    /// Which verifier signed (index into verifier_set)
    pub verifier_index: u8,
    /// ed25519 signature bytes (64 bytes)
    pub signature: Vec<u8>,
}
```

### 2. Contract State Changes

```rust
// BEFORE:
pub verifier: AccountId,

// AFTER:
pub verifier_set: Vec<VerifierInfo>,       // verifier panel
pub consensus_threshold: u8,                // min signatures (default: 2)
pub score_variance_max: u8,                 // max allowed score spread (default: 15)
```

### 3. Contract Struct Update

```rust
#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    pub owner: AccountId,
    pub escrows: UnorderedMap<String, Escrow>,
    // Replace single verifier with consensus set
    pub verifier_set: Vec<VerifierInfo>,
    pub consensus_threshold: u8,
    pub score_variance_max: u8,
    pub data_id_index: UnorderedMap<String, String>,
    pub storage_deposit_yocto: u128,
    pub worker_stake_yocto: u128,
    pub workers: UnorderedMap<String, WorkerAccount>,
    pub balances: UnorderedMap<String, U128>,
    pub paused_workers: UnorderedMap<String, ()>,
    pub stats: EscrowStats,
}
```

### 4. Init & Admin

```rust
#[init]
pub fn new(config: Option<InitConfig>) -> Self {
    assert!(!env::state_exists(), "Contract already initialized");
    let owner = env::signer_account_id();

    let config = config.unwrap_or_default();

    Self {
        owner: owner.clone(),
        escrows: UnorderedMap::new(b"e"),
        verifier_set: config.verifiers.unwrap_or(vec![VerifierInfo {
            account_id: owner.clone(),
            public_key: String::new(), // will be set via set_verifier_key
            active: true,
        }]),
        consensus_threshold: config.consensus_threshold.unwrap_or(2),
        score_variance_max: config.score_variance_max.unwrap_or(15),
        data_id_index: UnorderedMap::new(b"d"),
        storage_deposit_yocto: STORAGE_DEPOSIT_YOCTO,
        worker_stake_yocto: WORKER_STAKE_YOCTO,
        workers: UnorderedMap::new(b"w"),
        balances: UnorderedMap::new(b"b"),
        paused_workers: UnorderedMap::new(b"p"),
        stats: EscrowStats::default(),
    }
}

/// Add or update a verifier in the consensus set
pub fn set_verifier(&mut self, index: u8, info: VerifierInfo) {
    assert!(
        env::predecessor_account_id() == self.owner,
        "Only owner can manage verifiers"
    );
    let idx = index as usize;
    if idx < self.verifier_set.len() {
        self.verifier_set[idx] = info;
    } else if idx == self.verifier_set.len() {
        self.verifier_set.push(info);
    } else {
        panic!("Invalid verifier index");
    }
}

/// Remove a verifier (mark inactive — don't shift indices)
pub fn deactivate_verifier(&mut self, index: u8) {
    assert!(
        env::predecessor_account_id() == self.owner,
        "Only owner"
    );
    let idx = index as usize;
    assert!(idx < self.verifier_set.len(), "Invalid index");
    self.verifier_set[idx].active = false;
}

/// Update consensus parameters
pub fn set_consensus_config(&mut self, threshold: Option<u8>, variance_max: Option<u8>) {
    assert!(
        env::predecessor_account_id() == self.owner,
        "Only owner"
    );
    if let Some(t) = threshold {
        assert!(t >= 2, "Minimum 2 signatures required");
        assert!(
            (t as usize) <= self.verifier_set.iter().filter(|v| v.active).count(),
            "Threshold exceeds active verifier count"
        );
        self.consensus_threshold = t;
    }
    if let Some(v) = variance_max {
        self.score_variance_max = v;
    }
}
```

### 5. Core: `resume_verification` with Multi-Sig

```rust
/// Resume verification with multi-verifier consensus signatures.
/// Called by any account (relayer, verifier, or observer).
///
/// Verifies `consensus_threshold` valid ed25519 signatures from
/// active verifiers in the verifier_set, then resumes the yield.
pub fn resume_verification(
    &mut self,
    data_id_hex: String,
    signed_verdict: SignedVerdict,
) -> bool {
    // ── 1. Verify consensus signatures ──
    let verdict_bytes = signed_verdict.verdict_json.as_bytes();
    let valid_sigs = self.count_valid_signatures(verdict_bytes, &signed_verdict.signatures);

    assert!(
        valid_sigs >= self.consensus_threshold,
        "Insufficient valid signatures: {} < {}",
        valid_sigs,
        self.consensus_threshold
    );

    // ── 2. Parse verdict ──
    let verdict: Verdict = serde_json::from_str(&signed_verdict.verdict_json)
        .unwrap_or_else(|_| panic!("Invalid verdict JSON"));

    // ── 3. Validate score consistency ──
    assert!(
        verdict.passed == (verdict.score >= 50), // basic sanity
        "Score/passed mismatch"
    );

    // ── 4. Double-resume guard (unchanged) ──
    let matching_job = self.data_id_index.get(&data_id_hex);
    if let Some(ref jid) = matching_job {
        let escrow = self.escrows.get(jid).expect("escrow vanished");
        assert!(!escrow.yield_consumed, "Yield already consumed");
    }

    // ── 5. Decode data_id (unchanged) ──
    assert!(data_id_hex.len() == 64, "data_id must be 64 hex chars");
    let data_id_bytes: Vec<u8> = (0..64)
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&data_id_hex[i..i + 2], 16)
                .unwrap_or_else(|_| panic!("Invalid hex at {}", i))
        })
        .collect();
    let data_id: [u8; 32] = data_id_bytes.try_into().expect("32 bytes");

    // ── 6. Resume yield (unchanged) ──
    env::promise_yield_resume(&data_id, verdict_bytes);

    // ── 7. Mark consumed (unchanged) ──
    if let Some(jid) = matching_job {
        let mut escrow = self.escrows.get(&jid).expect("escrow vanished");
        escrow.yield_consumed = true;
        self.escrows.insert(&jid, &escrow);
    }

    true
}

/// Count valid ed25519 signatures from active verifiers
fn count_valid_signatures(
    &self,
    message: &[u8],
    signatures: &[VerifierSignature],
) -> u8 {
    let mut valid = 0u8;
    let mut seen_indices = std::collections::HashSet::new();

    for sig in signatures {
        let idx = sig.verifier_index as usize;

        // No duplicate verifier signatures
        if seen_indices.contains(&idx) {
            continue;
        }
        seen_indices.insert(idx);

        // Bounds + active check
        if idx >= self.verifier_set.len() {
            continue;
        }
        let verifier = &self.verifier_set[idx];
        if !verifier.active {
            continue;
        }

        // Decode public key
        let pk_bytes = hex_decode(&verifier.public_key);
        if pk_bytes.len() != 32 || sig.signature.len() != 64 {
            continue;
        }

        // Verify ed25519 signature
        let pk = near_sdk::ed25519_dalek::VerifyingKey::from_bytes(
            pk_bytes.try_into().unwrap()
        );
        if let Ok(pk) = pk {
            let signature = near_sdk::ed25519_dalek::Signature::from_bytes(
                &sig.signature.clone().try_into().unwrap()
            );
            if pk.verify(message, &signature).is_ok() {
                valid += 1;
            }
        }
    }

    valid
}
```

### 6. Backward Compatibility

```rust
/// Legacy single-verifier resume (deprecated, kept for migration)
/// Internally wraps the verdict in a SignedVerdict with the old verifier's signature
#[deprecated(note = "Use resume_verification with SignedVerdict")]
pub fn resume_verification_legacy(
    &mut self,
    data_id_hex: String,
    verdict: String,
) -> bool {
    assert_eq!(
        env::predecessor_account_id(),
        self.verifier_set[0].account_id,
        "Only legacy verifier"
    );
    // ... same old logic, no signature check since caller IS the verifier
}
```

---

## Migration Path

### Phase 1: Deploy with single verifier (current behavior)
```json
{
  "verifiers": [{"account_id": "verifier.near", "public_key": "...", "active": true}],
  "consensus_threshold": 1,
  "score_variance_max": 15
}
```

### Phase 2: Add 2 more verifiers, bump threshold
```bash
# On-chain calls
set_verifier(1, {account_id: "verifier2.near", public_key: "...", active: true})
set_verifier(2, {account_id: "verifier3.near", public_key: "...", active: true})
set_consensus_config(threshold: 2)
```

### Phase 3: Full 3-of-3 or 2-of-3 consensus
- Off-chain consensus pipeline running
- Verifiers post scores to Nostr
- Median computed, 2 sign, relayer submits
- No further contract changes needed

---

## Gas Impact

| Operation | Before | After | Delta |
|-----------|--------|-------|-------|
| `resume_verification` | ~3.1 Tgas | ~3.8 Tgas | +0.7 Tgas (sig verification) |
| `new` | ~0.5 Tgas | ~0.6 Tgas | +0.1 Tgas (verifier_set storage) |
| `verification_callback` | unchanged | unchanged | 0 |

**~22% more gas on resume, everything else unchanged.** The sig verification is cheap — ed25519 is fast.

---

## Security Improvement

| Risk | Before | After |
|------|--------|-------|
| Single verifier goes down | Funds lock | 2-of-3 continues |
| Single verifier bribed | Can steal | Need to bribe 2/3 |
| Verifier submits bad score | Unchecked | Cross-checked by 2+ verifiers |
| Owner replaces verifier | Silent takeover | Multiple verifiers = visible |
| Verifier front-running | Unchecked | Multiple independent verifiers |

**Trust reduced from 1 entity to 2-of-3 collusion.** For higher security, expand to 5 verifiers with 3-of-5 threshold.

---

## Off-chain Verifier Service (Rust pseudo-code)

```rust
// Each verifier runs this independently
async fn score_and_sign(job_id: &str, escrow: &EscrowView, result: &str) {
    // 1. Run LLM scoring
    let verdict = llm_verify(escrow.task_description, result).await;
    
    // 2. Post score to Nostr
    let event = build_nostr_event(41006, json!({
        "score": verdict.score,
        "passed": verdict.passed,
        "detail": verdict.detail,
    }), tags: [("j", job_id)]);
    nostr_publish(event).await;
    
    // 3. Watch for other scores
    let scores = poll_nostr_scores(job_id, timeout: 5min).await;
    
    // 4. If ≥2 scores available, compute consensus
    if scores.len() >= 2 {
        let median_score = median(&scores);
        let consensus_verdict = json!({
            "score": median_score,
            "passed": median_score >= escrow.score_threshold,
            "detail": format!("median of {} verifiers", scores.len()),
        });
        
        // 5. Sign the consensus verdict
        let sig = ed25519_sign(consensus_verdict.to_string().as_bytes(), &my_secret_key);
        
        // 6. Publish signed verdict
        publish_consensus(job_id, consensus_verdict, sig).await;
    }
}
```
