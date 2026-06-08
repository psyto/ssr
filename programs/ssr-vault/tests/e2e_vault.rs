//! End-to-end test for the collateral vault primitive.
//!
//! Walks through:
//!   1. Compliance bootstrap and a verified depositor.
//!   2. Plain Token-2022 mint (Phase 2 = Model C, no TransferHook).
//!   3. `init_vault` — vault PDA + canonical vault ATA owned by the PDA.
//!   4. Two `deposit`s (verifies idempotent position create + aggregation).
//!   5. Partial then full `withdraw` (verifies bookkeeping + Token-2022
//!      transfer signed by the vault PDA).
//!   6. Negative paths: suspended depositor → withdraw rejects;
//!      over-withdraw rejects.
//!
//! Prerequisite: `target/deploy/ssr_compliance.so` and
//! `target/deploy/ssr_vault.so` must exist (`cargo build-sbf`).

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
    ssr_types::{asset_class, compliance_status, seeds, Position, Vault},
    ssr_vault::{err as vault_err, ix as vault_ix},
    std::path::PathBuf,
};

// ─── Harness ─────────────────────────────────────────────────────────────

fn compliance_program_id() -> Address {
    Address::from([7u8; 32])
}
fn vault_program_id() -> Address {
    Address::from([9u8; 32])
}
fn token_2022_id() -> Address {
    spl_token_2022::ID.to_bytes().into()
}

const ATA_PROGRAM_ID: Address = Address::new_from_array(pinocchio_pubkey::from_str(
    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
));

fn so_path(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../target/deploy/");
    p.push(name);
    p
}

fn setup() -> Option<(LiteSVM, Keypair)> {
    for (label, path) in [
        ("ssr_compliance.so", so_path("ssr_compliance.so")),
        ("ssr_vault.so", so_path("ssr_vault.so")),
    ] {
        if !path.exists() {
            eprintln!(
                "SKIP: {label} not built. Run `cargo build-sbf` on each program first."
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
        vault_program_id(),
        &std::fs::read(so_path("ssr_vault.so")).unwrap(),
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

// ─── Derivations ─────────────────────────────────────────────────────────

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
fn derive_canonical_ata(owner: &Address, mint: &Address) -> Address {
    Address::find_program_address(
        &[owner.as_ref(), token_2022_id().as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

// ─── Compliance instruction builders (mirrors smoke.rs) ─────────────────-

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
            AccountMeta::new_readonly(*payer, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(derive_registry(), false),
            AccountMeta::new(derive_record(participant), false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data,
    }
}
fn ix_update_status(payer: &Address, participant: &Address, status: u8) -> Instruction {
    Instruction {
        program_id: compliance_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*payer, true),
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

// ─── Token-2022 helpers (mirror e2e_dvp.rs's no-hook flavor) ────────────-

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

fn create_user_ata(
    svm: &mut LiteSVM,
    payer: &Keypair,
    mint: &Address,
    owner: &Address,
) -> Address {
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

fn balance(svm: &LiteSVM, token_account: &Address) -> u64 {
    let acc = svm.get_account(token_account).unwrap();
    TokenAccount::unpack(&acc.data[..TokenAccount::LEN])
        .unwrap()
        .amount
}

// ─── Vault instruction builders ──────────────────────────────────────────

fn ix_init_vault(admin: &Address, mint: &Address) -> Instruction {
    Instruction {
        program_id: vault_program_id(),
        accounts: vec![
            AccountMeta::new(*admin, true),
            AccountMeta::new(derive_vault(mint), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data: vec![vault_ix::INIT_VAULT],
    }
}

#[allow(clippy::too_many_arguments)]
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

fn ix_withdraw(
    depositor: &Address,
    mint: &Address,
    vault_ata: &Address,
    depositor_ata: &Address,
    amount: u64,
) -> Instruction {
    let vault = derive_vault(mint);
    let mut data = Vec::with_capacity(1 + 8);
    data.push(vault_ix::WITHDRAW);
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: vault_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*depositor, true),
            AccountMeta::new_readonly(derive_record(depositor), false),
            AccountMeta::new_readonly(compliance_program_id(), false),
            AccountMeta::new(vault, false),
            AccountMeta::new(derive_position(&vault, depositor), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*vault_ata, false),
            AccountMeta::new(*depositor_ata, false),
            AccountMeta::new_readonly(token_2022_id(), false),
        ],
        data,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[test]
fn full_lifecycle_deposit_partial_withdraw_full_withdraw() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };

    // Compliance bootstrap.
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    // Token-2022 mint (plain).
    let mint_kp = Keypair::new();
    create_plain_mint(&mut svm, &payer, &mint_kp, &payer.pubkey(), 6);
    let mint = mint_kp.pubkey();

    // Depositor + verify.
    let depositor = Keypair::new();
    svm.airdrop(&depositor.pubkey(), 1_000_000_000).unwrap();
    send(
        &mut svm,
        &[ix_register_account(&payer.pubkey(), &depositor.pubkey())],
        &[&payer],
    );
    send(
        &mut svm,
        &[ix_update_status(
            &payer.pubkey(),
            &depositor.pubkey(),
            compliance_status::VERIFIED,
        )],
        &[&payer],
    );

    // Depositor ATA + mint tokens to it.
    let depositor_ata = create_user_ata(&mut svm, &payer, &mint, &depositor.pubkey());
    mint_to(&mut svm, &payer, &mint, &payer, &depositor_ata, 1_000_000);

    // init_vault — admin = payer.
    send(&mut svm, &[ix_init_vault(&payer.pubkey(), &mint)], &[&payer]);
    let vault = derive_vault(&mint);
    // Vault account exists and decodes.
    let v_acc = svm.get_account(&vault).expect("vault PDA exists");
    let v: &Vault = from_bytes(&v_acc.data[..Vault::LEN]);
    assert_eq!(v.mint, mint.to_bytes());
    assert_eq!(v.admin, payer.pubkey().to_bytes());
    assert_eq!(v.total_deposited, 0);
    assert_eq!(v.position_count, 0);

    // Create the vault's canonical ATA — same path the deposit relies on.
    let vault_ata = create_user_ata(&mut svm, &payer, &mint, &vault);

    // First deposit: 500_000. Should idempotently create the position.
    send(
        &mut svm,
        &[ix_deposit(
            &depositor.pubkey(),
            &payer.pubkey(),
            &mint,
            &depositor_ata,
            &vault_ata,
            500_000,
        )],
        &[&payer, &depositor],
    );
    assert_eq!(balance(&svm, &vault_ata), 500_000);
    let v_acc = svm.get_account(&vault).unwrap();
    let v: &Vault = from_bytes(&v_acc.data[..Vault::LEN]);
    assert_eq!(v.total_deposited, 500_000);
    assert_eq!(v.position_count, 1);
    let position = derive_position(&vault, &depositor.pubkey());
    let p_acc = svm.get_account(&position).expect("position PDA exists");
    let p: &Position = from_bytes(&p_acc.data[..Position::LEN]);
    assert_eq!(p.amount_deposited, 500_000);
    assert_eq!(p.locked_amount, 0);
    assert_eq!(p.available(), 500_000);

    // Second deposit: 300_000. Position aggregates.
    send(
        &mut svm,
        &[ix_deposit(
            &depositor.pubkey(),
            &payer.pubkey(),
            &mint,
            &depositor_ata,
            &vault_ata,
            300_000,
        )],
        &[&payer, &depositor],
    );
    let v_acc = svm.get_account(&vault).unwrap();
    let v: &Vault = from_bytes(&v_acc.data[..Vault::LEN]);
    assert_eq!(v.total_deposited, 800_000);
    assert_eq!(v.position_count, 1, "second deposit must not bump count");
    let p_acc = svm.get_account(&position).unwrap();
    let p: &Position = from_bytes(&p_acc.data[..Position::LEN]);
    assert_eq!(p.amount_deposited, 800_000);

    // Partial withdraw: 200_000.
    send(
        &mut svm,
        &[ix_withdraw(
            &depositor.pubkey(),
            &mint,
            &vault_ata,
            &depositor_ata,
            200_000,
        )],
        &[&payer, &depositor],
    );
    assert_eq!(balance(&svm, &vault_ata), 600_000);
    let p_acc = svm.get_account(&position).unwrap();
    let p: &Position = from_bytes(&p_acc.data[..Position::LEN]);
    assert_eq!(p.amount_deposited, 600_000);

    // Full withdraw of remainder: 600_000.
    send(
        &mut svm,
        &[ix_withdraw(
            &depositor.pubkey(),
            &mint,
            &vault_ata,
            &depositor_ata,
            600_000,
        )],
        &[&payer, &depositor],
    );
    assert_eq!(balance(&svm, &vault_ata), 0);
    let p_acc = svm.get_account(&position).unwrap();
    let p: &Position = from_bytes(&p_acc.data[..Position::LEN]);
    assert_eq!(p.amount_deposited, 0);
    assert_eq!(p.available(), 0);
    let v_acc = svm.get_account(&vault).unwrap();
    let v: &Vault = from_bytes(&v_acc.data[..Vault::LEN]);
    assert_eq!(v.total_deposited, 0);
    assert_eq!(v.position_count, 1, "position not auto-closed at zero (Phase 2)");
}

#[test]
fn withdraw_rejects_when_depositor_suspended() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    let mint_kp = Keypair::new();
    create_plain_mint(&mut svm, &payer, &mint_kp, &payer.pubkey(), 6);
    let mint = mint_kp.pubkey();

    let depositor = Keypair::new();
    svm.airdrop(&depositor.pubkey(), 1_000_000_000).unwrap();
    send(
        &mut svm,
        &[ix_register_account(&payer.pubkey(), &depositor.pubkey())],
        &[&payer],
    );
    send(
        &mut svm,
        &[ix_update_status(
            &payer.pubkey(),
            &depositor.pubkey(),
            compliance_status::VERIFIED,
        )],
        &[&payer],
    );

    let depositor_ata = create_user_ata(&mut svm, &payer, &mint, &depositor.pubkey());
    mint_to(&mut svm, &payer, &mint, &payer, &depositor_ata, 1_000_000);

    send(&mut svm, &[ix_init_vault(&payer.pubkey(), &mint)], &[&payer]);
    let vault = derive_vault(&mint);
    let vault_ata = create_user_ata(&mut svm, &payer, &mint, &vault);

    send(
        &mut svm,
        &[ix_deposit(
            &depositor.pubkey(),
            &payer.pubkey(),
            &mint,
            &depositor_ata,
            &vault_ata,
            500_000,
        )],
        &[&payer, &depositor],
    );

    // Suspend the depositor *after* they deposited. Withdraw must
    // reject — even though the tokens are technically theirs.
    send(
        &mut svm,
        &[ix_update_status(
            &payer.pubkey(),
            &depositor.pubkey(),
            compliance_status::SUSPENDED,
        )],
        &[&payer],
    );

    let res = try_send(
        &mut svm,
        &[ix_withdraw(
            &depositor.pubkey(),
            &mint,
            &vault_ata,
            &depositor_ata,
            500_000,
        )],
        &[&payer, &depositor],
    );
    assert!(res.is_err(), "withdraw must reject when depositor suspended");
    assert_eq!(balance(&svm, &vault_ata), 500_000, "tokens stuck in vault");
}

#[test]
fn over_withdraw_rejects() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    let mint_kp = Keypair::new();
    create_plain_mint(&mut svm, &payer, &mint_kp, &payer.pubkey(), 6);
    let mint = mint_kp.pubkey();
    let depositor = Keypair::new();
    svm.airdrop(&depositor.pubkey(), 1_000_000_000).unwrap();
    send(
        &mut svm,
        &[ix_register_account(&payer.pubkey(), &depositor.pubkey())],
        &[&payer],
    );
    send(
        &mut svm,
        &[ix_update_status(
            &payer.pubkey(),
            &depositor.pubkey(),
            compliance_status::VERIFIED,
        )],
        &[&payer],
    );
    let depositor_ata = create_user_ata(&mut svm, &payer, &mint, &depositor.pubkey());
    mint_to(&mut svm, &payer, &mint, &payer, &depositor_ata, 1_000_000);
    send(&mut svm, &[ix_init_vault(&payer.pubkey(), &mint)], &[&payer]);
    let vault = derive_vault(&mint);
    let vault_ata = create_user_ata(&mut svm, &payer, &mint, &vault);
    send(
        &mut svm,
        &[ix_deposit(
            &depositor.pubkey(),
            &payer.pubkey(),
            &mint,
            &depositor_ata,
            &vault_ata,
            100_000,
        )],
        &[&payer, &depositor],
    );

    // Try to withdraw more than was deposited.
    let res = try_send(
        &mut svm,
        &[ix_withdraw(
            &depositor.pubkey(),
            &mint,
            &vault_ata,
            &depositor_ata,
            500_000,
        )],
        &[&payer, &depositor],
    );
    assert!(res.is_err(), "over-withdraw must reject");
    assert_eq!(balance(&svm, &vault_ata), 100_000);
}

// Touch the error namespace import for the compiler to confirm we use it.
#[allow(dead_code)]
fn _use_vault_err() {
    let _ = vault_err::INSUFFICIENT_AVAILABLE;
    let _ = vault_err::COMPLIANCE_SUSPENDED;
}

// ─── Phase 4 v0: asset_class round-trip ─────────────────────────────────
//
// The vault's on-chain init_vault now reads an optional trailing byte
// from instruction data and persists it as `Vault::asset_class`. This
// pins:
//   - Backwards compat: 1-byte ix data (the legacy shape) yields
//     `asset_class::UNKNOWN`, so any pre-Phase-4 caller keeps working
//     and that vault contributes zero margin credit.
//   - New shape: explicit 2-byte ix data persists the class verbatim.
// If anyone removes the `rest` threading or the `unwrap_or(UNKNOWN)`
// from process_init_vault, both halves of this test must fail.

fn ix_init_vault_with_class(admin: &Address, mint: &Address, class: u8) -> Instruction {
    Instruction {
        program_id: vault_program_id(),
        accounts: vec![
            AccountMeta::new(*admin, true),
            AccountMeta::new(derive_vault(mint), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data: vec![vault_ix::INIT_VAULT, class],
    }
}

#[test]
fn init_vault_defaults_asset_class_to_unknown_when_data_is_legacy() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let mint_kp = Keypair::new();
    create_plain_mint(&mut svm, &payer, &mint_kp, &payer.pubkey(), 6);
    let mint = mint_kp.pubkey();
    // Legacy shape: 1-byte ix data (just the discriminator).
    send(&mut svm, &[ix_init_vault(&payer.pubkey(), &mint)], &[&payer]);
    let v_acc = svm.get_account(&derive_vault(&mint)).unwrap();
    let v: Vault = *from_bytes(&v_acc.data[..Vault::LEN]);
    assert_eq!(
        v.asset_class,
        asset_class::UNKNOWN,
        "legacy 1-byte init_vault data must yield UNKNOWN (zero margin credit)"
    );
}

#[test]
fn init_vault_persists_explicit_asset_class() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let mint_kp = Keypair::new();
    create_plain_mint(&mut svm, &payer, &mint_kp, &payer.pubkey(), 6);
    let mint = mint_kp.pubkey();
    send(
        &mut svm,
        &[ix_init_vault_with_class(&payer.pubkey(), &mint, asset_class::EQUITY)],
        &[&payer],
    );
    let v_acc = svm.get_account(&derive_vault(&mint)).unwrap();
    let v: Vault = *from_bytes(&v_acc.data[..Vault::LEN]);
    assert_eq!(
        v.asset_class,
        asset_class::EQUITY,
        "explicit asset_class byte must round-trip into Vault::asset_class"
    );
}
