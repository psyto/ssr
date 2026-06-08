//! End-to-end demo dramaturgy test — the full pipeline a live demo
//! attendee would see, executed in-process via LiteSVM.
//!
//! Mirrors `cli/README.md` "Demo dramaturgy" steps 1-13:
//!
//!   1. Compliance bootstrap (`initialize_registry`).
//!   2. Onboard + verify two participants (borrower, lender).
//!   3. Two plain Token-2022 mints (asset = collateral, cash) with
//!      initial inventory minted to each side.
//!   4. SPC `CreateDvp` with the ssr-dvp-wrapper PDA as
//!      `settlement_authority`; both parties fund their escrow leg.
//!   5. `compliant_settle_dvp` atomically delivers asset ↔ cash through
//!      the wrapper after the compliance gate accepts both legs.
//!   6. Vault init for both mints (admin step).
//!   7. Each party deposits their remaining post-DvP balance into the
//!      vault.
//!   8. `open_repo` locks both positions; `close_repo` unlocks.
//!   9. `open_loan` locks collateral AND transfers cash within the
//!      vault (Phase 3b); `repay_loan` reverses both.
//!  10. Negative: post-onboarding suspension blocks `open_loan` with
//!      `COMPLIANCE_SUSPENDED` (`0x5013`).
//!  11. Negative-then-positive: warping past `maturity_slot` makes
//!      `repay_loan` reject with `MATURED` (`0x5022`) and the lender
//!      can recover collateral via `liquidate_loan`.
//!
//! The point of running this end-to-end is not to re-verify what the
//! per-program tests already cover — it's to pin the cross-program
//! composition. A renamed PDA seed, a re-ordered account list, or a
//! discriminator shift in one program would silently break the demo
//! script even if every per-program test still passed; this test
//! surfaces that immediately.
//!
//! Prerequisites:
//!   * `cargo build-sbf` produces every `.so` under `target/deploy/`
//!     (ssr_compliance, ssr_dvp_wrapper, ssr_vault, ssr_repo,
//!     ssr_lending).
//!   * `tests/fixtures/dvp_swap_program.so` is the SPC artifact.
//!
//! Tests skip cleanly (without failing) if any artifact is missing.

use {
    bytemuck::from_bytes,
    litesvm::LiteSVM,
    solana_address::Address,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_message::Message,
    solana_program_pack::Pack,
    solana_signer::Signer,
    solana_transaction::Transaction,
    spl_token_2022::state::{Account as TokenAccount, Mint},
    ssr_dvp_wrapper::{AUTHORITY_SEED, SPC_DVP_PROGRAM_ID, ix as wrapper_ix},
    ssr_lending::ix as lending_ix,
    ssr_repo::ix as repo_ix,
    ssr_types::{
        asset_class, compliance_status, loan_status, repo_status, seeds, Loan, Position, Repo,
    },
    std::path::PathBuf,
};

// ─── vault discriminator mirror ─────────────────────────────────────────
// ssr-vault is not a Rust-level dep (see Cargo.toml); these mirror the
// canonical values in `programs/ssr-vault/src/lib.rs::ix` and are pinned
// by the drift check at the bottom of the file.
mod vault_ix {
    pub const INIT_VAULT: u8 = 0;
    pub const DEPOSIT: u8 = 1;
    #[allow(dead_code)]
    pub const WITHDRAW: u8 = 2;
    pub const LOCK_POSITION: u8 = 3;
    pub const UNLOCK_POSITION: u8 = 4;
    pub const TRANSFER_WITHIN_VAULT: u8 = 5;
    pub const SEIZE_LOCKED: u8 = 6;
    pub const INIT_POSITION: u8 = 7;
}

// ─── Program ID assignment ───────────────────────────────────────────────
// Synthetic byte-filled IDs for the SSR programs. The SPC dvp-swap
// program is loaded at its fixed canonical ID (its account-derivation
// macros bake that ID in, so we can't substitute).

fn compliance_program_id() -> Address {
    Address::from([7u8; 32])
}
fn wrapper_program_id() -> Address {
    Address::from([8u8; 32])
}
fn vault_program_id() -> Address {
    Address::from([9u8; 32])
}
fn repo_program_id() -> Address {
    Address::from([11u8; 32])
}
fn lending_program_id() -> Address {
    Address::from([13u8; 32])
}
fn spc_dvp_program_id() -> Address {
    Address::from(SPC_DVP_PROGRAM_ID)
}
fn token_2022_id() -> Address {
    spl_token_2022::ID.to_bytes().into()
}
const ATA_PROGRAM_ID: Address = Address::new_from_array(pinocchio_pubkey::from_str(
    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
));

// ─── Setup / send helpers ────────────────────────────────────────────────

fn so_path(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../target/deploy/");
    p.push(name);
    p
}

fn spc_dvp_so_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../fixtures/dvp_swap_program.so");
    p
}

fn setup() -> Option<(LiteSVM, Keypair)> {
    let artifacts: [(&str, PathBuf); 6] = [
        ("ssr_compliance.so", so_path("ssr_compliance.so")),
        ("ssr_dvp_wrapper.so", so_path("ssr_dvp_wrapper.so")),
        ("ssr_vault.so", so_path("ssr_vault.so")),
        ("ssr_repo.so", so_path("ssr_repo.so")),
        ("ssr_lending.so", so_path("ssr_lending.so")),
        ("dvp_swap_program.so", spc_dvp_so_path()),
    ];
    for (label, path) in &artifacts {
        if !path.exists() {
            eprintln!(
                "SKIP: {label} not found at {}.\n\
                 - For SSR *.so under target/deploy/, run \
                 `cargo build-sbf --manifest-path programs/<crate>/Cargo.toml` \
                 for each of ssr-compliance, ssr-dvp-wrapper, ssr-vault, \
                 ssr-repo, ssr-lending.\n\
                 - For dvp_swap_program.so under tests/fixtures/, see the \
                 SPC reference build instructions in docs/spc-integration.md.",
                path.display()
            );
            return None;
        }
    }

    let mut svm = LiteSVM::new();
    svm.add_program(
        compliance_program_id(),
        &std::fs::read(so_path("ssr_compliance.so")).unwrap(),
    )
    .unwrap();
    svm.add_program(
        wrapper_program_id(),
        &std::fs::read(so_path("ssr_dvp_wrapper.so")).unwrap(),
    )
    .unwrap();
    svm.add_program(
        vault_program_id(),
        &std::fs::read(so_path("ssr_vault.so")).unwrap(),
    )
    .unwrap();
    svm.add_program(
        repo_program_id(),
        &std::fs::read(so_path("ssr_repo.so")).unwrap(),
    )
    .unwrap();
    svm.add_program(
        lending_program_id(),
        &std::fs::read(so_path("ssr_lending.so")).unwrap(),
    )
    .unwrap();
    svm.add_program(
        spc_dvp_program_id(),
        &std::fs::read(spc_dvp_so_path()).unwrap(),
    )
    .unwrap();

    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    Some((svm, payer))
}

fn send(svm: &mut LiteSVM, ixs: &[Instruction], signers: &[&Keypair]) {
    let blockhash = svm.latest_blockhash();
    let payer = signers[0].pubkey();
    let msg = Message::new(ixs, Some(&payer));
    let tx = Transaction::new(signers, msg, blockhash);
    svm.send_transaction(tx).expect("tx failed");
}

fn try_send(
    svm: &mut LiteSVM,
    ixs: &[Instruction],
    signers: &[&Keypair],
) -> Result<(), litesvm::types::FailedTransactionMetadata> {
    let blockhash = svm.latest_blockhash();
    let payer = signers[0].pubkey();
    let msg = Message::new(ixs, Some(&payer));
    let tx = Transaction::new(signers, msg, blockhash);
    svm.send_transaction(tx).map(|_| ())
}

// ─── PDA derivations ─────────────────────────────────────────────────────

fn derive_registry() -> Address {
    Address::find_program_address(&[seeds::REGISTRY], &compliance_program_id()).0
}
fn derive_record(participant: &Address) -> Address {
    Address::find_program_address(
        &[seeds::ACCOUNT_RECORD, participant.as_ref()],
        &compliance_program_id(),
    )
    .0
}
fn derive_wrapper_authority() -> Address {
    Address::find_program_address(&[AUTHORITY_SEED], &wrapper_program_id()).0
}
fn derive_vault(mint: &Address) -> Address {
    Address::find_program_address(&[seeds::VAULT, mint.as_ref()], &vault_program_id()).0
}
fn derive_position(vault: &Address, depositor: &Address) -> Address {
    Address::find_program_address(
        &[seeds::POSITION, vault.as_ref(), depositor.as_ref()],
        &vault_program_id(),
    )
    .0
}
fn derive_repo(
    borrower: &Address,
    lender: &Address,
    collateral_vault: &Address,
    cash_vault: &Address,
    nonce: u64,
) -> Address {
    let nonce_bytes = nonce.to_le_bytes();
    Address::find_program_address(
        &[
            seeds::REPO,
            borrower.as_ref(),
            lender.as_ref(),
            collateral_vault.as_ref(),
            cash_vault.as_ref(),
            &nonce_bytes,
        ],
        &repo_program_id(),
    )
    .0
}
fn derive_loan(
    borrower: &Address,
    lender: &Address,
    collateral_vault: &Address,
    cash_vault: &Address,
    nonce: u64,
) -> Address {
    let nonce_bytes = nonce.to_le_bytes();
    Address::find_program_address(
        &[
            seeds::LOAN,
            borrower.as_ref(),
            lender.as_ref(),
            collateral_vault.as_ref(),
            cash_vault.as_ref(),
            &nonce_bytes,
        ],
        &lending_program_id(),
    )
    .0
}
fn derive_swap_dvp(
    settlement_authority: &Address,
    user_a: &Address,
    user_b: &Address,
    mint_a: &Address,
    mint_b: &Address,
    nonce: u64,
) -> Address {
    let nonce_bytes = nonce.to_le_bytes();
    Address::find_program_address(
        &[
            b"dvp",
            settlement_authority.as_ref(),
            user_a.as_ref(),
            user_b.as_ref(),
            mint_a.as_ref(),
            mint_b.as_ref(),
            &nonce_bytes,
        ],
        &spc_dvp_program_id(),
    )
    .0
}
fn derive_canonical_ata(owner: &Address, mint: &Address) -> Address {
    Address::find_program_address(
        &[owner.as_ref(), token_2022_id().as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

// ─── Compliance instruction builders ─────────────────────────────────────

fn ix_initialize_registry(payer: &Address) -> Instruction {
    Instruction {
        program_id: compliance_program_id(),
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(derive_registry(), false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data: vec![ssr_compliance::ix::INITIALIZE_REGISTRY],
    }
}

fn ix_register_account(payer: &Address, participant: &Address) -> Instruction {
    let mut data = Vec::with_capacity(1 + 32 + 2);
    data.push(ssr_compliance::ix::REGISTER_ACCOUNT);
    data.extend_from_slice(participant.as_ref());
    data.extend_from_slice(b"JP");
    Instruction {
        program_id: compliance_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*payer, true), // operator
            AccountMeta::new(*payer, true),          // payer
            AccountMeta::new_readonly(derive_registry(), false),
            AccountMeta::new(derive_record(participant), false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data,
    }
}

fn ix_update_status(operator: &Address, participant: &Address, status: u8) -> Instruction {
    Instruction {
        program_id: compliance_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*operator, true),
            AccountMeta::new_readonly(derive_registry(), false),
            AccountMeta::new(derive_record(participant), false),
        ],
        data: vec![
            ssr_compliance::ix::UPDATE_STATUS,
            status,
            0,
            ssr_compliance::change_mask::STATUS,
        ],
    }
}

// ─── Token-2022 helpers ──────────────────────────────────────────────────

fn create_plain_mint(
    svm: &mut LiteSVM,
    payer: &Keypair,
    mint_kp: &Keypair,
    mint_authority: &Address,
    decimals: u8,
) {
    let space = Mint::LEN;
    let lamports = svm.minimum_balance_for_rent_exemption(space);
    let create = solana_system_interface::instruction::create_account(
        &payer.pubkey(),
        &mint_kp.pubkey(),
        lamports,
        space as u64,
        &spl_token_2022::ID,
    );
    let init = spl_token_2022::instruction::initialize_mint2(
        &spl_token_2022::ID,
        &mint_kp.pubkey(),
        mint_authority,
        None,
        decimals,
    )
    .unwrap();
    send(svm, &[create, init], &[payer, mint_kp]);
}

fn create_user_ata(svm: &mut LiteSVM, payer: &Keypair, mint: &Address, owner: &Address) -> Address {
    let ata = derive_canonical_ata(owner, mint);
    let ix = Instruction {
        program_id: ATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new(ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
            AccountMeta::new_readonly(token_2022_id(), false),
        ],
        data: vec![1u8], // CreateIdempotent
    };
    send(svm, &[ix], &[payer]);
    ata
}

fn mint_to(
    svm: &mut LiteSVM,
    payer: &Keypair,
    mint: &Address,
    authority: &Keypair,
    dest: &Address,
    amount: u64,
) {
    let ix = spl_token_2022::instruction::mint_to(
        &spl_token_2022::ID,
        mint,
        dest,
        &authority.pubkey(),
        &[],
        amount,
    )
    .unwrap();
    send(svm, &[ix], &[payer, authority]);
}

fn transfer_spl(
    svm: &mut LiteSVM,
    payer: &Keypair,
    source: &Address,
    dest: &Address,
    owner: &Keypair,
    amount: u64,
) {
    let ix = spl_token_2022::instruction::transfer(
        &spl_token_2022::ID,
        source,
        dest,
        &owner.pubkey(),
        &[],
        amount,
    )
    .unwrap();
    send(svm, &[ix], &[payer, owner]);
}

fn ata_balance(svm: &LiteSVM, ata: &Address) -> u64 {
    let acc = svm.get_account(ata).unwrap();
    TokenAccount::unpack(&acc.data[..TokenAccount::LEN])
        .unwrap()
        .amount
}

// ─── SPC + wrapper instruction builders ──────────────────────────────────

const SPC_IX_CREATE_DVP: u8 = 0;

#[allow(clippy::too_many_arguments)]
fn ix_spc_create_dvp(
    payer: &Address,
    swap_dvp: &Address,
    settlement_authority: &Address,
    mint_a: &Address,
    mint_b: &Address,
    dvp_ata_a: &Address,
    dvp_ata_b: &Address,
    user_a: &Address,
    user_b: &Address,
    amount_a: u64,
    amount_b: u64,
    expiry_timestamp: i64,
    nonce: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(1 + 32 * 2 + 8 * 4 + 1);
    data.push(SPC_IX_CREATE_DVP);
    data.extend_from_slice(user_a.as_ref());
    data.extend_from_slice(user_b.as_ref());
    data.extend_from_slice(&amount_a.to_le_bytes());
    data.extend_from_slice(&amount_b.to_le_bytes());
    data.extend_from_slice(&expiry_timestamp.to_le_bytes());
    data.extend_from_slice(&nonce.to_le_bytes());
    data.push(0); // earliest_settlement_timestamp = None

    Instruction {
        program_id: spc_dvp_program_id(),
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(*swap_dvp, false),
            AccountMeta::new_readonly(*settlement_authority, false),
            AccountMeta::new_readonly(*mint_a, false),
            AccountMeta::new_readonly(*mint_b, false),
            AccountMeta::new(*dvp_ata_a, false),
            AccountMeta::new(*dvp_ata_b, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
            AccountMeta::new_readonly(token_2022_id(), false),
            AccountMeta::new_readonly(token_2022_id(), false),
            AccountMeta::new_readonly(ATA_PROGRAM_ID, false),
        ],
        data,
    }
}

#[allow(clippy::too_many_arguments)]
fn ix_compliant_settle_dvp(
    wrapper_authority: &Address,
    swap_dvp: &Address,
    mint_a: &Address,
    mint_b: &Address,
    dvp_ata_a: &Address,
    dvp_ata_b: &Address,
    user_a_ata_b: &Address,
    user_b_ata_a: &Address,
    user_a_ata_a: &Address,
    user_b_ata_b: &Address,
    user_a_record: &Address,
    user_b_record: &Address,
    leg_a_extras_count: u8,
) -> Instruction {
    Instruction {
        program_id: wrapper_program_id(),
        accounts: vec![
            AccountMeta::new(*wrapper_authority, false),
            AccountMeta::new_readonly(compliance_program_id(), false),
            AccountMeta::new_readonly(spc_dvp_program_id(), false),
            AccountMeta::new_readonly(*user_a_record, false),
            AccountMeta::new_readonly(*user_b_record, false),
            AccountMeta::new(*swap_dvp, false),
            AccountMeta::new_readonly(*mint_a, false),
            AccountMeta::new_readonly(*mint_b, false),
            AccountMeta::new(*dvp_ata_a, false),
            AccountMeta::new(*dvp_ata_b, false),
            AccountMeta::new(*user_a_ata_b, false),
            AccountMeta::new(*user_b_ata_a, false),
            AccountMeta::new(*user_a_ata_a, false),
            AccountMeta::new(*user_b_ata_b, false),
            AccountMeta::new_readonly(token_2022_id(), false),
            AccountMeta::new_readonly(token_2022_id(), false),
        ],
        data: vec![wrapper_ix::COMPLIANT_SETTLE_DVP, leg_a_extras_count],
    }
}

// ─── Vault instruction builders ──────────────────────────────────────────

fn ix_init_vault(admin: &Address, mint: &Address, asset_class: u8) -> Instruction {
    Instruction {
        program_id: vault_program_id(),
        accounts: vec![
            AccountMeta::new(*admin, true),
            AccountMeta::new(derive_vault(mint), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data: vec![vault_ix::INIT_VAULT, asset_class],
    }
}

fn ix_initialize_risk_params(admin: &Address, payer: &Address) -> Instruction {
    let risk = Address::find_program_address(&[seeds::RISK_PARAMS], &compliance_program_id()).0;
    Instruction {
        program_id: compliance_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(derive_registry(), false),
            AccountMeta::new(risk, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data: vec![ssr_compliance::ix::INITIALIZE_RISK_PARAMS],
    }
}

fn derive_risk_params() -> Address {
    Address::find_program_address(&[seeds::RISK_PARAMS], &compliance_program_id()).0
}

fn derive_price_feed(mint: &Address) -> Address {
    Address::find_program_address(&[seeds::PRICE_FEED, mint.as_ref()], &compliance_program_id()).0
}

fn ix_register_price_feed(
    admin: &Address,
    payer: &Address,
    mint: &Address,
    price_micro_usd: u64,
    mint_decimals: u8,
) -> Instruction {
    let feed = derive_price_feed(mint);
    let mut data = Vec::with_capacity(1 + 32 + 8 + 1);
    data.push(ssr_compliance::ix::REGISTER_PRICE_FEED);
    data.extend_from_slice(mint.as_ref());
    data.extend_from_slice(&price_micro_usd.to_le_bytes());
    data.push(mint_decimals);
    Instruction {
        program_id: compliance_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(derive_registry(), false),
            AccountMeta::new(feed, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data,
    }
}

fn ix_deposit(
    depositor: &Address,
    payer: &Address,
    mint: &Address,
    depositor_ata: &Address,
    vault_ata: &Address,
    amount: u64,
) -> Instruction {
    let vault = derive_vault(mint);
    let mut data = Vec::with_capacity(1 + 8);
    data.push(vault_ix::DEPOSIT);
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: vault_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*depositor, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(derive_record(depositor), false),
            AccountMeta::new_readonly(compliance_program_id(), false),
            AccountMeta::new(vault, false),
            AccountMeta::new(derive_position(&vault, depositor), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*depositor_ata, false),
            AccountMeta::new(*vault_ata, false),
            AccountMeta::new_readonly(token_2022_id(), false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data,
    }
}

fn ix_init_position(depositor: &Address, payer: &Address, mint: &Address) -> Instruction {
    let vault = derive_vault(mint);
    Instruction {
        program_id: vault_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*depositor, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(vault, false),
            AccountMeta::new(derive_position(&vault, depositor), false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data: vec![vault_ix::INIT_POSITION],
    }
}

// ─── Repo instruction builders ───────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn ix_open_repo(
    borrower: &Address,
    lender: &Address,
    payer: &Address,
    collateral_mint: &Address,
    cash_mint: &Address,
    collateral_amount: u64,
    cash_amount: u64,
    expiry_slot: u64,
    nonce: u64,
) -> Instruction {
    let collateral_vault = derive_vault(collateral_mint);
    let cash_vault = derive_vault(cash_mint);
    let collateral_position = derive_position(&collateral_vault, borrower);
    let cash_position = derive_position(&cash_vault, lender);
    let repo = derive_repo(borrower, lender, &collateral_vault, &cash_vault, nonce);
    let mut data = Vec::with_capacity(1 + 32);
    data.push(repo_ix::OPEN_REPO);
    data.extend_from_slice(&collateral_amount.to_le_bytes());
    data.extend_from_slice(&cash_amount.to_le_bytes());
    data.extend_from_slice(&expiry_slot.to_le_bytes());
    data.extend_from_slice(&nonce.to_le_bytes());
    Instruction {
        program_id: repo_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*borrower, true),
            AccountMeta::new_readonly(*lender, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(derive_record(borrower), false),
            AccountMeta::new_readonly(derive_record(lender), false),
            AccountMeta::new_readonly(compliance_program_id(), false),
            AccountMeta::new_readonly(vault_program_id(), false),
            AccountMeta::new_readonly(collateral_vault, false),
            AccountMeta::new(collateral_position, false),
            AccountMeta::new_readonly(cash_vault, false),
            AccountMeta::new(cash_position, false),
            AccountMeta::new(repo, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data,
    }
}

fn ix_close_repo(
    borrower: &Address,
    lender: &Address,
    collateral_mint: &Address,
    cash_mint: &Address,
    nonce: u64,
) -> Instruction {
    let collateral_vault = derive_vault(collateral_mint);
    let cash_vault = derive_vault(cash_mint);
    let collateral_position = derive_position(&collateral_vault, borrower);
    let cash_position = derive_position(&cash_vault, lender);
    let repo = derive_repo(borrower, lender, &collateral_vault, &cash_vault, nonce);
    Instruction {
        program_id: repo_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*borrower, true),
            AccountMeta::new_readonly(vault_program_id(), false),
            AccountMeta::new_readonly(collateral_vault, false),
            AccountMeta::new(collateral_position, false),
            AccountMeta::new_readonly(cash_vault, false),
            AccountMeta::new(cash_position, false),
            AccountMeta::new(repo, false),
        ],
        data: vec![repo_ix::CLOSE_REPO],
    }
}

// ─── Lending instruction builders ────────────────────────────────────────

fn derive_loan_list(borrower: &Address) -> Address {
    Address::find_program_address(
        &[seeds::LOAN_LIST, borrower.as_ref()],
        &lending_program_id(),
    )
    .0
}

#[allow(clippy::too_many_arguments)]
fn ix_open_loan(
    borrower: &Address,
    lender: &Address,
    payer: &Address,
    collateral_mint: &Address,
    cash_mint: &Address,
    collateral_amount: u64,
    principal_amount: u64,
    maturity_slot: u64,
    nonce: u64,
    interest_bps_per_year: u32,
    extra_positions: &[(Address, Address)],
    existing_loans: &[Address],
    price_feed_mints: &[Address],
    existing_loan_cash_vaults: &[Address],
) -> Instruction {
    let collateral_vault = derive_vault(collateral_mint);
    let cash_vault = derive_vault(cash_mint);
    let collateral_position = derive_position(&collateral_vault, borrower);
    let lender_cash_position = derive_position(&cash_vault, lender);
    let borrower_cash_position = derive_position(&cash_vault, borrower);
    let loan = derive_loan(borrower, lender, &collateral_vault, &cash_vault, nonce);
    let mut data = Vec::with_capacity(1 + 39);
    data.push(lending_ix::OPEN_LOAN);
    data.extend_from_slice(&collateral_amount.to_le_bytes());
    data.extend_from_slice(&principal_amount.to_le_bytes());
    data.extend_from_slice(&maturity_slot.to_le_bytes());
    data.extend_from_slice(&nonce.to_le_bytes());
    data.extend_from_slice(&interest_bps_per_year.to_le_bytes());
    data.push(extra_positions.len() as u8);
    data.push(price_feed_mints.len() as u8);
    data.push(existing_loans.len() as u8);
    let mut accounts = vec![
        AccountMeta::new_readonly(*borrower, true),
        AccountMeta::new_readonly(*lender, true),
        AccountMeta::new(*payer, true),
        AccountMeta::new_readonly(derive_record(borrower), false),
        AccountMeta::new_readonly(derive_record(lender), false),
        AccountMeta::new_readonly(compliance_program_id(), false),
        AccountMeta::new_readonly(vault_program_id(), false),
        AccountMeta::new_readonly(collateral_vault, false),
        AccountMeta::new(collateral_position, false),
        AccountMeta::new_readonly(cash_vault, false),
        AccountMeta::new(lender_cash_position, false),
        AccountMeta::new(borrower_cash_position, false),
        AccountMeta::new(loan, false),
        AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        AccountMeta::new(derive_loan_list(borrower), false),
        AccountMeta::new_readonly(derive_risk_params(), false),
    ];
    for (pos, vault) in extra_positions {
        accounts.push(AccountMeta::new_readonly(*pos, false));
        accounts.push(AccountMeta::new_readonly(*vault, false));
    }
    for loan in existing_loans {
        accounts.push(AccountMeta::new_readonly(*loan, false));
    }
    for cv in existing_loan_cash_vaults {
        accounts.push(AccountMeta::new_readonly(*cv, false));
    }
    let mut feeds: Vec<Address> = price_feed_mints.iter().map(derive_price_feed).collect();
    feeds.sort();
    for feed in feeds {
        accounts.push(AccountMeta::new_readonly(feed, false));
    }
    Instruction {
        program_id: lending_program_id(),
        accounts,
        data,
    }
}

fn ix_repay_loan(
    borrower: &Address,
    lender: &Address,
    collateral_mint: &Address,
    cash_mint: &Address,
    nonce: u64,
) -> Instruction {
    let collateral_vault = derive_vault(collateral_mint);
    let cash_vault = derive_vault(cash_mint);
    let collateral_position = derive_position(&collateral_vault, borrower);
    let borrower_cash_position = derive_position(&cash_vault, borrower);
    let lender_cash_position = derive_position(&cash_vault, lender);
    let loan = derive_loan(borrower, lender, &collateral_vault, &cash_vault, nonce);
    Instruction {
        program_id: lending_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*borrower, true),
            AccountMeta::new_readonly(vault_program_id(), false),
            AccountMeta::new_readonly(collateral_vault, false),
            AccountMeta::new(collateral_position, false),
            AccountMeta::new_readonly(cash_vault, false),
            AccountMeta::new(borrower_cash_position, false),
            AccountMeta::new(lender_cash_position, false),
            AccountMeta::new(loan, false),
            AccountMeta::new(derive_loan_list(borrower), false),
        ],
        data: vec![lending_ix::REPAY_LOAN],
    }
}

fn ix_liquidate_loan(
    borrower: &Address,
    lender: &Address,
    collateral_mint: &Address,
    cash_mint: &Address,
    nonce: u64,
) -> Instruction {
    let collateral_vault = derive_vault(collateral_mint);
    let cash_vault = derive_vault(cash_mint);
    let borrower_collateral = derive_position(&collateral_vault, borrower);
    let lender_collateral = derive_position(&collateral_vault, lender);
    let loan = derive_loan(borrower, lender, &collateral_vault, &cash_vault, nonce);
    Instruction {
        program_id: lending_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*lender, true),
            AccountMeta::new_readonly(vault_program_id(), false),
            AccountMeta::new_readonly(collateral_vault, false),
            AccountMeta::new(borrower_collateral, false),
            AccountMeta::new(lender_collateral, false),
            AccountMeta::new(loan, false),
            AccountMeta::new(derive_loan_list(borrower), false),
        ],
        data: vec![lending_ix::LIQUIDATE_LOAN],
    }
}

// ─── Shared fixture: state after compliance + DvP completes ──────────────
//
// The fixture leaves the world in a state where:
//   - registry is initialized, both parties are VERIFIED
//   - asset_mint + cash_mint exist with both parties' ATAs
//   - DvP has settled: borrower (user_a) has 800_000 asset + 200_000
//     cash; lender (user_b) has 800_000 cash + 200_000 asset
//   - asset_vault and cash_vault are initialized with their ATAs created
//   - borrower has deposited 500_000 asset into asset_vault
//   - lender has deposited 400_000 cash into cash_vault
//   - lender's collateral_position and borrower's cash_position are
//     pre-created so the Phase 3b lending path doesn't need to do it
//     mid-transaction

struct Fixture {
    payer: Keypair,
    borrower: Keypair, // = DvP user_a, holds the asset (collateral)
    lender: Keypair,   // = DvP user_b, holds the cash
    asset_mint: Address,
    cash_mint: Address,
}

fn build_fixture(svm: &mut LiteSVM, payer: Keypair) -> Fixture {
    // 1. Compliance bootstrap + v1c RiskParams init. v1b open_loan
    // requires RiskParams to exist (haircut table source-of-truth),
    // so every fresh deployment must run init-risk-params before any
    // lending traffic. The demo dramaturgy treats this as a step-0
    // operator action — same shape as the registry init.
    send(svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    send(
        svm,
        &[ix_initialize_risk_params(&payer.pubkey(), &payer.pubkey())],
        &[&payer],
    );
    // v1d: register PriceFeeds for both mints. Demo uses $1.00 / 6d
    // for both — same trivial FX as the lending crate's e2e suite,
    // so the demo's 200k-cash / 300k-collateral numbers continue to
    // match the v1b account-credit math while the gate is now
    // oracle-driven on chain. The mints come from a Keypair::new() in
    // step 2-3 below; register_price_feed runs AFTER those mints
    // exist.

    // 2-3. Two plain Token-2022 mints (Model C — no TransferHook).
    let asset_kp = Keypair::new();
    let cash_kp = Keypair::new();
    create_plain_mint(svm, &payer, &asset_kp, &payer.pubkey(), 6);
    create_plain_mint(svm, &payer, &cash_kp, &payer.pubkey(), 6);
    let asset_mint = asset_kp.pubkey();
    let cash_mint = cash_kp.pubkey();

    // v1d: register PriceFeeds for both mints (both priced at $1.00).
    send(
        svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &asset_mint, 1_000_000, 6)],
        &[&payer],
    );
    send(
        svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &cash_mint, 1_000_000, 6)],
        &[&payer],
    );

    // Two participants, both registered + verified.
    let borrower = Keypair::new();
    let lender = Keypair::new();
    svm.airdrop(&borrower.pubkey(), 1_000_000_000).unwrap();
    svm.airdrop(&lender.pubkey(), 1_000_000_000).unwrap();
    for u in [&borrower.pubkey(), &lender.pubkey()] {
        send(svm, &[ix_register_account(&payer.pubkey(), u)], &[&payer]);
        send(
            svm,
            &[ix_update_status(
                &payer.pubkey(),
                u,
                compliance_status::VERIFIED,
            )],
            &[&payer],
        );
    }

    // All four ATAs (each party holds both mints — cash side post-DvP
    // is the receive-side ATA that SPC SettleDvp credits).
    let borrower_asset_ata = create_user_ata(svm, &payer, &asset_mint, &borrower.pubkey());
    let borrower_cash_ata = create_user_ata(svm, &payer, &cash_mint, &borrower.pubkey());
    let lender_asset_ata = create_user_ata(svm, &payer, &asset_mint, &lender.pubkey());
    let lender_cash_ata = create_user_ata(svm, &payer, &cash_mint, &lender.pubkey());

    // Initial inventory: borrower starts with asset, lender with cash.
    mint_to(svm, &payer, &asset_mint, &payer, &borrower_asset_ata, 1_000_000);
    mint_to(svm, &payer, &cash_mint, &payer, &lender_cash_ata, 1_000_000);

    // 4. SPC CreateDvp with wrapper PDA as settlement_authority.
    let wrapper_authority = derive_wrapper_authority();
    let dvp_nonce = 1u64;
    let dvp_amount = 200_000u64; // each leg
    let expiry = 9_999_999_999i64;
    let swap_dvp = derive_swap_dvp(
        &wrapper_authority,
        &borrower.pubkey(),
        &lender.pubkey(),
        &asset_mint,
        &cash_mint,
        dvp_nonce,
    );
    let dvp_escrow_asset = derive_canonical_ata(&swap_dvp, &asset_mint);
    let dvp_escrow_cash = derive_canonical_ata(&swap_dvp, &cash_mint);
    send(
        svm,
        &[ix_spc_create_dvp(
            &payer.pubkey(),
            &swap_dvp,
            &wrapper_authority,
            &asset_mint,
            &cash_mint,
            &dvp_escrow_asset,
            &dvp_escrow_cash,
            &borrower.pubkey(),
            &lender.pubkey(),
            dvp_amount,
            dvp_amount,
            expiry,
            dvp_nonce,
        )],
        &[&payer],
    );

    // Each side funds its leg.
    transfer_spl(
        svm,
        &payer,
        &borrower_asset_ata,
        &dvp_escrow_asset,
        &borrower,
        dvp_amount,
    );
    transfer_spl(
        svm,
        &payer,
        &lender_cash_ata,
        &dvp_escrow_cash,
        &lender,
        dvp_amount,
    );

    // 5. compliant_settle_dvp — wrapper verifies both records, CPIs SettleDvp.
    let borrower_record = derive_record(&borrower.pubkey());
    let lender_record = derive_record(&lender.pubkey());
    send(
        svm,
        &[ix_compliant_settle_dvp(
            &wrapper_authority,
            &swap_dvp,
            &asset_mint,
            &cash_mint,
            &dvp_escrow_asset,
            &dvp_escrow_cash,
            &borrower_cash_ata,
            &lender_asset_ata,
            &borrower_asset_ata,
            &lender_cash_ata,
            &borrower_record,
            &lender_record,
            0,
        )],
        &[&payer],
    );

    // Post-DvP invariants — pin the demo's punchline value.
    assert_eq!(ata_balance(svm, &borrower_asset_ata), 800_000);
    assert_eq!(ata_balance(svm, &borrower_cash_ata), 200_000);
    assert_eq!(ata_balance(svm, &lender_asset_ata), 200_000);
    assert_eq!(ata_balance(svm, &lender_cash_ata), 800_000);
    assert!(
        svm.get_account(&swap_dvp).is_none()
            || svm.get_account(&swap_dvp).unwrap().lamports == 0,
        "swap_dvp closed after settle"
    );

    // 6. Vault init for both mints + canonical ATA for each vault PDA.
    // Use SOVEREIGN_BOND for the asset side (500 bps haircut → real
    // margin credit) and STABLECOIN for cash (0 bps) so v1b's margin
    // gate sees enough credit for the demo's 200_000 loan against
    // 500_000 of locked collateral.
    send(
        svm,
        &[ix_init_vault(&payer.pubkey(), &asset_mint, asset_class::SOVEREIGN_BOND)],
        &[&payer],
    );
    create_user_ata(svm, &payer, &asset_mint, &derive_vault(&asset_mint));
    send(
        svm,
        &[ix_init_vault(&payer.pubkey(), &cash_mint, asset_class::STABLECOIN)],
        &[&payer],
    );
    create_user_ata(svm, &payer, &cash_mint, &derive_vault(&cash_mint));

    // 7. Each party deposits their post-DvP balance side. Borrower
    // deposits 600_000 (bumped from 500_000 vs pre-v1b) so the
    // conservative margin gate has enough collateral headroom for
    // the canonical 300k-lock / 200k-borrow demo: 600k × 0.95 −
    // 300k × 0.95 = 285k pool, comfortably covers the 200k liab.
    let asset_vault_ata = derive_canonical_ata(&derive_vault(&asset_mint), &asset_mint);
    let cash_vault_ata = derive_canonical_ata(&derive_vault(&cash_mint), &cash_mint);
    send(
        svm,
        &[ix_deposit(
            &borrower.pubkey(),
            &payer.pubkey(),
            &asset_mint,
            &borrower_asset_ata,
            &asset_vault_ata,
            600_000,
        )],
        &[&payer, &borrower],
    );
    send(
        svm,
        &[ix_deposit(
            &lender.pubkey(),
            &payer.pubkey(),
            &cash_mint,
            &lender_cash_ata,
            &cash_vault_ata,
            400_000,
        )],
        &[&payer, &lender],
    );

    // Phase 3b: pre-create the cross-side positions that open_loan /
    // liquidate_loan write into. open_loan credits borrower's cash;
    // liquidate_loan credits lender's collateral.
    send(
        svm,
        &[ix_init_position(&borrower.pubkey(), &payer.pubkey(), &cash_mint)],
        &[&payer, &borrower],
    );
    send(
        svm,
        &[ix_init_position(&lender.pubkey(), &payer.pubkey(), &asset_mint)],
        &[&payer, &lender],
    );

    Fixture {
        payer,
        borrower,
        lender,
        asset_mint,
        cash_mint,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[test]
fn demo_full_pipeline_compliance_dvp_vault_repo_lending() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = build_fixture(&mut svm, payer);

    // ── Repo: lock both sides, then unlock. ──
    let repo_nonce = 1u64;
    let asset_vault = derive_vault(&f.asset_mint);
    let cash_vault = derive_vault(&f.cash_mint);
    let borrower_collateral = derive_position(&asset_vault, &f.borrower.pubkey());
    let lender_cash = derive_position(&cash_vault, &f.lender.pubkey());
    let repo = derive_repo(
        &f.borrower.pubkey(),
        &f.lender.pubkey(),
        &asset_vault,
        &cash_vault,
        repo_nonce,
    );

    send(
        &mut svm,
        &[ix_open_repo(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.asset_mint,
            &f.cash_mint,
            300_000,
            200_000,
            u64::MAX,
            repo_nonce,
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );

    let cp: Position =
        *from_bytes(&svm.get_account(&borrower_collateral).unwrap().data[..Position::LEN]);
    assert_eq!(cp.locked_amount, 300_000, "borrower collateral locked by repo");
    assert_eq!(
        cp.lock_authority,
        repo.to_bytes(),
        "repo PDA holds the collateral lock"
    );
    let lc: Position = *from_bytes(&svm.get_account(&lender_cash).unwrap().data[..Position::LEN]);
    assert_eq!(lc.locked_amount, 200_000, "lender cash locked by repo");
    assert_eq!(
        lc.lock_authority,
        repo.to_bytes(),
        "repo PDA holds the cash lock"
    );
    let repo_data = svm.get_account(&repo).unwrap().data;
    let r: Repo = *from_bytes(&repo_data[..Repo::LEN]);
    assert_eq!(r.status, repo_status::OPEN);
    // Phase 4 v1a: `ssr-cli margin show` enumerates open repos via
    // getProgramAccounts memcmp at borrower offset 8 and lender offset
    // 40 (status filtered client-side). Pin the on-wire bytes here so
    // the CLI's filter can't silently mismatch what the program wrote.
    assert_eq!(&repo_data[8..40], f.borrower.pubkey().to_bytes().as_slice());
    assert_eq!(&repo_data[40..72], f.lender.pubkey().to_bytes().as_slice());

    send(
        &mut svm,
        &[ix_close_repo(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.asset_mint,
            &f.cash_mint,
            repo_nonce,
        )],
        &[&f.payer, &f.borrower],
    );

    let cp: Position =
        *from_bytes(&svm.get_account(&borrower_collateral).unwrap().data[..Position::LEN]);
    assert_eq!(cp.locked_amount, 0, "collateral unlocked after close");
    assert_eq!(cp.lock_authority, [0u8; 32], "lock_authority cleared");
    let r: Repo = *from_bytes(&svm.get_account(&repo).unwrap().data[..Repo::LEN]);
    assert_eq!(r.status, repo_status::CLOSED);

    // ── Lending: lock collateral + transfer cash, then repay. ──
    let loan_nonce = 1u64;
    let interest_bps_per_year = 0u32; // 0% to keep the repay math obvious
    let borrower_cash = derive_position(&cash_vault, &f.borrower.pubkey());
    let loan = derive_loan(
        &f.borrower.pubkey(),
        &f.lender.pubkey(),
        &asset_vault,
        &cash_vault,
        loan_nonce,
    );

    send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.asset_mint,
            &f.cash_mint,
            300_000,
            200_000,
            u64::MAX,
            loan_nonce,
            interest_bps_per_year,
            &[],
            &[],
            &[f.asset_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );

    let cp: Position =
        *from_bytes(&svm.get_account(&borrower_collateral).unwrap().data[..Position::LEN]);
    assert_eq!(cp.locked_amount, 300_000);
    assert_eq!(cp.lock_authority, loan.to_bytes(), "loan PDA owns the lock");
    let lc: Position = *from_bytes(&svm.get_account(&lender_cash).unwrap().data[..Position::LEN]);
    assert_eq!(lc.amount_deposited, 200_000, "lender cash drained by principal");
    let bc: Position = *from_bytes(&svm.get_account(&borrower_cash).unwrap().data[..Position::LEN]);
    assert_eq!(bc.amount_deposited, 200_000, "borrower received principal");
    let loan_data = svm.get_account(&loan).unwrap().data;
    let l: Loan = *from_bytes(&loan_data[..Loan::LEN]);
    assert_eq!(l.status, loan_status::OPEN);
    // Same memcmp-offset pin as the repo case above — `margin show`
    // reads borrower at 8, lender at 40 to enumerate open loans.
    assert_eq!(&loan_data[8..40], f.borrower.pubkey().to_bytes().as_slice());
    assert_eq!(&loan_data[40..72], f.lender.pubkey().to_bytes().as_slice());

    send(
        &mut svm,
        &[ix_repay_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.asset_mint,
            &f.cash_mint,
            loan_nonce,
        )],
        &[&f.payer, &f.borrower],
    );

    let cp: Position =
        *from_bytes(&svm.get_account(&borrower_collateral).unwrap().data[..Position::LEN]);
    assert_eq!(cp.locked_amount, 0, "collateral unlocked after repay");
    assert_eq!(cp.lock_authority, [0u8; 32]);
    let lc: Position = *from_bytes(&svm.get_account(&lender_cash).unwrap().data[..Position::LEN]);
    assert_eq!(lc.amount_deposited, 400_000, "lender cash restored to pre-loan");
    let bc: Position = *from_bytes(&svm.get_account(&borrower_cash).unwrap().data[..Position::LEN]);
    assert_eq!(bc.amount_deposited, 0, "borrower cash returned to lender");
    let l: Loan = *from_bytes(&svm.get_account(&loan).unwrap().data[..Loan::LEN]);
    assert_eq!(l.status, loan_status::REPAID);
}

#[test]
fn open_loan_rejects_when_borrower_suspended_post_onboarding() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = build_fixture(&mut svm, payer);

    // Same fixture state as the happy path. Now the compliance team
    // discovers something post-onboarding and flips the borrower to
    // SUSPENDED *after* they've already deposited collateral. The
    // demo's punchline is that the lending gate still catches them
    // with a distinct error (not a generic vault reject).
    send(
        &mut svm,
        &[ix_update_status(
            &f.payer.pubkey(),
            &f.borrower.pubkey(),
            compliance_status::SUSPENDED,
        )],
        &[&f.payer],
    );

    let res = try_send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.asset_mint,
            &f.cash_mint,
            300_000,
            200_000,
            u64::MAX,
            1,
            500,
            &[],
            &[],
            &[f.asset_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );
    assert!(
        res.is_err(),
        "open_loan must reject when borrower is suspended"
    );
}

#[test]
fn past_maturity_repay_rejects_then_liquidate_recovers_collateral() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = build_fixture(&mut svm, payer);

    let loan_nonce = 7u64;
    let maturity_slot = 1_000u64; // small absolute so we can warp past it
    let asset_vault = derive_vault(&f.asset_mint);
    let cash_vault = derive_vault(&f.cash_mint);
    let borrower_collateral = derive_position(&asset_vault, &f.borrower.pubkey());
    let lender_collateral = derive_position(&asset_vault, &f.lender.pubkey());
    let loan = derive_loan(
        &f.borrower.pubkey(),
        &f.lender.pubkey(),
        &asset_vault,
        &cash_vault,
        loan_nonce,
    );

    send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.asset_mint,
            &f.cash_mint,
            300_000,
            200_000,
            maturity_slot,
            loan_nonce,
            500,
            &[],
            &[],
            &[f.asset_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );

    // Confirm the open landed before we warp past maturity.
    let bc_pre: Position =
        *from_bytes(&svm.get_account(&borrower_collateral).unwrap().data[..Position::LEN]);
    assert_eq!(bc_pre.locked_amount, 300_000);
    assert_eq!(bc_pre.amount_deposited, 600_000);
    let lc_pre: Position =
        *from_bytes(&svm.get_account(&lender_collateral).unwrap().data[..Position::LEN]);
    assert_eq!(
        lc_pre.amount_deposited, 0,
        "lender collateral position empty before liquidation"
    );

    svm.warp_to_slot(maturity_slot + 10);

    // repay must reject with MATURED so the liquidate path is the
    // only way to recover.
    let repay_res = try_send(
        &mut svm,
        &[ix_repay_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.asset_mint,
            &f.cash_mint,
            loan_nonce,
        )],
        &[&f.payer, &f.borrower],
    );
    assert!(
        repay_res.is_err(),
        "repay must reject after maturity_slot (Phase 3b: only liquidate clears collateral)"
    );

    // liquidate succeeds: collateral moves from borrower-locked to
    // lender-unlocked.
    send(
        &mut svm,
        &[ix_liquidate_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.asset_mint,
            &f.cash_mint,
            loan_nonce,
        )],
        &[&f.payer, &f.lender],
    );

    let bc: Position =
        *from_bytes(&svm.get_account(&borrower_collateral).unwrap().data[..Position::LEN]);
    assert_eq!(bc.locked_amount, 0, "borrower collateral lock cleared");
    assert_eq!(
        bc.amount_deposited,
        300_000,
        "borrower keeps the unencumbered portion (600_000 - 300_000 liquidated)"
    );
    assert_eq!(bc.lock_authority, [0u8; 32]);
    let lc: Position =
        *from_bytes(&svm.get_account(&lender_collateral).unwrap().data[..Position::LEN]);
    assert_eq!(
        lc.amount_deposited, 300_000,
        "lender receives the seized collateral"
    );
    assert_eq!(lc.locked_amount, 0);
    let l: Loan = *from_bytes(&svm.get_account(&loan).unwrap().data[..Loan::LEN]);
    assert_eq!(l.status, loan_status::LIQUIDATED);
}

// ─── Drift check: pin the vault-ix discriminators we mirrored. ──────────
//
// The cross-program test hard-codes ssr-vault's discriminator bytes (see
// `mod vault_ix` at the top of this file) instead of taking a Rust
// dependency on `ssr-vault`. If anyone renumbers the ix enum in
// `programs/ssr-vault/src/lib.rs`, this test must update in lockstep —
// the per-program drift checks in ssr-repo / ssr-lending will also fire,
// but pinning here too keeps the failure local to the integration suite.

#[test]
fn vault_ix_discriminator_drift_check() {
    assert_eq!(vault_ix::INIT_VAULT, 0);
    assert_eq!(vault_ix::DEPOSIT, 1);
    assert_eq!(vault_ix::WITHDRAW, 2);
    assert_eq!(vault_ix::LOCK_POSITION, 3);
    assert_eq!(vault_ix::UNLOCK_POSITION, 4);
    assert_eq!(vault_ix::TRANSFER_WITHIN_VAULT, 5);
    assert_eq!(vault_ix::SEIZE_LOCKED, 6);
    assert_eq!(vault_ix::INIT_POSITION, 7);
}
