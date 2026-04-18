#!/bin/bash
# deploy.sh — Deploy escrow + agent-msig to testnet or mainnet
#
# Usage:
#   ./scripts/deploy.sh testnet          # Deploy both contracts to testnet
#   ./scripts/deploy.sh mainnet          # Deploy both contracts to mainnet
#   ./scripts/deploy.sh testnet escrow   # Deploy only escrow
#   ./scripts/deploy.sh testnet msig     # Deploy only msig
#
# Prerequisites:
#   - near-cli-rs installed and logged in
#   - cargo-near installed (cargo install cargo-near)
#   - Funded account for deployment

set -euo pipefail

NETWORK="${1:-testnet}"
COMPONENT="${2:-all}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

echo -e "${BLUE}═══════════════════════════════════════════════${NC}"
echo -e "${BLUE}  NEAR Escrow + Agent MSig Deployment${NC}"
echo -e "${BLUE}  Network: ${NETWORK} | Component: ${COMPONENT}${NC}"
echo -e "${BLUE}═══════════════════════════════════════════════${NC}"

# ---- Configuration ----

# Change these before deploying
ESCROW_ACCOUNT="${DEPLOY_ESCROW_ACCOUNT:-}"
MSIG_ACCOUNT="${DEPLOY_MSIG_ACCOUNT:-}"
SIGNER_ACCOUNT="${DEPLOY_SIGNER_ACCOUNT:-}"

# Verifier keys (generate with: openssl rand -hex 32)
VERIFIER_1_ACCOUNT="${VERIFIER_1_ACCOUNT:-verifier1.example.near}"
VERIFIER_1_PK="${VERIFIER_1_PK:-}"
VERIFIER_2_ACCOUNT="${VERIFIER_2_ACCOUNT:-verifier2.example.near}"
VERIFIER_2_PK="${VERIFIER_2_PK:-}"
VERIFIER_3_ACCOUNT="${VERIFIER_3_ACCOUNT:-verifier3.example.near}"
VERIFIER_3_PK="${VERIFIER_3_PK:-}"

# Agent keypair for msig (generate with: openssl rand -hex 32)
AGENT_PUBLIC_KEY="${AGENT_PUBLIC_KEY:-}"
AGENT_NPUB="${AGENT_NPUB:-}"

# FT token to allow (empty = accept all)
ALLOWED_TOKENS="${ALLOWED_TOKENS:-[]}"

# ---- Validation ----

validate_config() {
    local missing=()

    if [[ -z "$ESCROW_ACCOUNT" ]]; then missing+=("DEPLOY_ESCROW_ACCOUNT"); fi
    if [[ -z "$MSIG_ACCOUNT" ]]; then missing+=("DEPLOY_MSIG_ACCOUNT"); fi
    if [[ -z "$SIGNER_ACCOUNT" ]]; then missing+=("DEPLOY_SIGNER_ACCOUNT"); fi

    if [[ "$COMPONENT" == "all" || "$COMPONENT" == "escrow" ]]; then
        if [[ -z "$VERIFIER_1_PK" || -z "$VERIFIER_2_PK" || -z "$VERIFIER_3_PK" ]]; then
            missing+=("VERIFIER_1_PK / VERIFIER_2_PK / VERIFIER_3_PK (hex ed25519 public keys)")
        fi
    fi

    if [[ "$COMPONENT" == "all" || "$COMPONENT" == "msig" ]]; then
        if [[ -z "$AGENT_PUBLIC_KEY" ]]; then
            missing+=("AGENT_PUBLIC_KEY (ed25519:base58...)")
        fi
    fi

    if [[ ${#missing[@]} -gt 0 ]]; then
        echo -e "${RED}Missing configuration:${NC}"
        for m in "${missing[@]}"; do
            echo -e "  ${RED}• ${m}${NC}"
        done
        echo ""
        echo "Set environment variables or edit this script."
        echo ""
        echo "Generate verifier keys:"
        echo "  for i in 1 2 3; do echo \"VERIFIER_\${i}_PK=\$(openssl rand -hex 32)\"; done"
        echo ""
        echo "Generate agent key:"
        echo "  near generate-key agent-msig --networkId ${NETWORK}"
        exit 1
    fi
}

# ---- Build ----

build_contracts() {
    echo -e "${YELLOW}Building contracts...${NC}"

    if [[ "$COMPONENT" == "all" || "$COMPONENT" == "escrow" ]]; then
        echo "  Building escrow..."
        cd "$ROOT_DIR"
        cargo near build --release --no-docker 2>&1 | tail -3
    fi

    if [[ "$COMPONENT" == "all" || "$COMPONENT" == "msig" ]]; then
        echo "  Building agent-msig..."
        cd "$ROOT_DIR/agent-msig"
        cargo near build --release --no-docker 2>&1 | tail -3
    fi

    echo -e "${GREEN}Build complete.${NC}"
}

# ---- Deploy ----

deploy_escrow() {
    echo -e "${YELLOW}Deploying escrow to ${ESCROW_ACCOUNT}...${NC}"

    local wasm="$ROOT_DIR/target/near/escrow_contract.wasm"
    if [[ ! -f "$wasm" ]]; then
        wasm="$ROOT_DIR/target/wasm32-unknown-unknown/release/near_escrow.wasm"
    fi

    if [[ ! -f "$wasm" ]]; then
        echo -e "${RED}Escrow WASM not found. Run build first.${NC}"
        exit 1
    fi

    # Deploy contract code
    near contract deploy "$ESCROW_ACCOUNT" use-file "$wasm" \
        with-init-call "new" \
        json-args "{
            \"verifier_set\": [
                {\"account_id\": \"${VERIFIER_1_ACCOUNT}\", \"public_key\": \"${VERIFIER_1_PK}\", \"active\": true},
                {\"account_id\": \"${VERIFIER_2_ACCOUNT}\", \"public_key\": \"${VERIFIER_2_PK}\", \"active\": true},
                {\"account_id\": \"${VERIFIER_3_ACCOUNT}\", \"public_key\": \"${VERIFIER_3_PK}\", \"active\": true}
            ],
            \"consensus_threshold\": 2,
            \"allowed_tokens\": ${ALLOWED_TOKENS}
        }" \
        prepaid-gas '100 Tgas' \
        attached-deposit '0 NEAR' \
        sign-as "$SIGNER_ACCOUNT" \
        network-config "$NETWORK" \
        send

    echo -e "${GREEN}Escrow deployed: ${ESCROW_ACCOUNT}${NC}"
}

deploy_msig() {
    echo -e "${YELLOW}Deploying agent-msig to ${MSIG_ACCOUNT}...${NC}"

    local wasm="$ROOT_DIR/agent-msig/target/near/agent_msig.wasm"
    if [[ ! -f "$wasm" ]]; then
        wasm="$ROOT_DIR/target/wasm32-unknown-unknown/release/agent_msig.wasm"
    fi

    if [[ ! -f "$wasm" ]]; then
        echo -e "${RED}MSig WASM not found. Run build first.${NC}"
        exit 1
    fi

    # Deploy contract code
    near contract deploy "$MSIG_ACCOUNT" use-file "$wasm" \
        with-init-call "new" \
        json-args "{
            \"agent_pubkey\": \"${AGENT_PUBLIC_KEY}\",
            \"agent_npub\": \"${AGENT_NPUB}\",
            \"escrow_contract\": \"${ESCROW_ACCOUNT}\"
        }" \
        prepaid-gas '30 Tgas' \
        attached-deposit '0 NEAR' \
        sign-as "$SIGNER_ACCOUNT" \
        network-config "$NETWORK" \
        send

    echo -e "${GREEN}MSig deployed: ${MSIG_ACCOUNT}${NC}"
}

# ---- Post-deploy verification ----

verify_deployment() {
    echo ""
    echo -e "${YELLOW}Verifying deployment...${NC}"

    if [[ "$COMPONENT" == "all" || "$COMPONENT" == "escrow" ]]; then
        echo "  Escrow state:"
        near contract call-function as-readonly "$ESCROW_ACCOUNT" "get_verifier_set" json-args '{}' network-config "$NETWORK" send 2>/dev/null || true
        echo "  Consensus threshold:"
        near contract call-function as-readonly "$ESCROW_ACCOUNT" "is_paused" json-args '{}' network-config "$NETWORK" send 2>/dev/null || true
    fi

    if [[ "$COMPONENT" == "all" || "$COMPONENT" == "msig" ]]; then
        echo "  MSig escrow target:"
        near contract call-function as-readonly "$MSIG_ACCOUNT" "get_escrow_contract" json-args '{}' network-config "$NETWORK" send 2>/dev/null || true
        echo "  MSig nonce:"
        near contract call-function as-readonly "$MSIG_ACCOUNT" "get_nonce" json-args '{}' network-config "$NETWORK" send 2>/dev/null || true
    fi

    echo ""
    echo -e "${GREEN}═══════════════════════════════════════════════${NC}"
    echo -e "${GREEN}  Deployment complete!${NC}"
    echo -e "${GREEN}═══════════════════════════════════════════════${NC}"
    echo ""
    echo "  Escrow:  ${ESCROW_ACCOUNT}"
    echo "  MSig:    ${MSIG_ACCOUNT}"
    echo "  Network: ${NETWORK}"
    echo ""
    echo "  Next steps:"
    echo "    1. Fund the msig with NEAR + FT tokens"
    echo "    2. Register msig in escrow's FT (storage_deposit)"
    echo "    3. Start verifier service with keys for index 0, 1, 2"
    echo "    4. Test with: agent → msig.execute(create_escrow) → worker.claim → ..."
}

# ---- Main ----

validate_config
build_contracts

if [[ "$COMPONENT" == "all" || "$COMPONENT" == "escrow" ]]; then
    deploy_escrow
fi

if [[ "$COMPONENT" == "all" || "$COMPONENT" == "msig" ]]; then
    deploy_msig
fi

verify_deployment
