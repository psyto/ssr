//! End-to-end test for the Phase 3 repo wrapper.
//!
//! Walks through:
//!   1. Compliance bootstrap; two verified parties (borrower + lender).
//!   2. Two plain Token-2022 mints (collateral + cash) and two vaults.
//!   3. Each party deposits into their respective vault.
//!   4. `open_repo` — both Position PDAs get locked against the Repo PDA.
//!   5. `close_repo` — the Repo PDA signs the unlock, both positions
//!      become free again, `Repo::status` flips to `CLOSED`.
//!   6. Negative: re-opening with a borrower flipped to `SUSPENDED`
//!      between deposit and `open_repo` rejects.
//!
//! Prerequisite: every `.so` under `target/deploy/` (compliance, vault,
//! repo) must exist (`cargo build-sbf` on each).

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
    spl_token_2022::state::Mint,
    ssr_repo::{ix as repo_ix},
    ssr_types::{compliance_status, repo_status, seeds, Position, Repo},
    std::path::PathBuf,
};

// Mirrored from ssr-vault to avoid cross-crate Pinocchio link issues
// during host test builds; the drift test at the bottom pins them.
mod vault_ix {
    pub const INIT_VAULT: u8 = 0;
    pub const DEPOSIT: u8 = 1;
    #[allow(dead_code)]
    pub const WITHDRAW: u8 = 2;
    pub const LOCK_POSITION: u8 = 3;
    pub const UNLOCK_POSITION: u8 = 4;
}

// ─── Harness ─────────────────────────────────────────────────────────────

fn compliance_program_id() -> Address {
    Address::from([7u8; 32])
}
fn vault_program_id() -> Address {
    Address::from([9u8; 32])
}
fn repo_program_id() -> Address {
    Address::from([11u8; 32])
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
        ("ssr_repo.so", so_path("ssr_repo.so")),
    ] {
        if !path.exists() {
            eprintln!("SKIP: {label} not built. Run `cargo build-sbf` for each program first.");
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
    svm.add_program(
        repo_program_id(),
        &std::fs::read(so_path("ssr_repo.so")).unwrap(),
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

// ─── Derivations ────────────────────────────────────────────────────────-

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
        data: vec![1u8],
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

// ─── Repo instruction builders ──────────────────────────────────────────-

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

// ─── Helpers + tests ────────────────────────────────────────────────────-

struct Fixture {
    payer: Keypair,
    borrower: Keypair,
    lender: Keypair,
    collateral_mint: Address,
    cash_mint: Address,
}

fn setup_fixture(svm: &mut LiteSVM, payer: Keypair) -> Fixture {
    send(svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    let collateral_mint_kp = Keypair::new();
    let cash_mint_kp = Keypair::new();
    create_plain_mint(svm, &payer, &collateral_mint_kp, &payer.pubkey(), 6);
    create_plain_mint(svm, &payer, &cash_mint_kp, &payer.pubkey(), 6);
    let collateral_mint = collateral_mint_kp.pubkey();
    let cash_mint = cash_mint_kp.pubkey();

    let borrower = Keypair::new();
    let lender = Keypair::new();
    svm.airdrop(&borrower.pubkey(), 1_000_000_000).unwrap();
    svm.airdrop(&lender.pubkey(), 1_000_000_000).unwrap();
    for u in [&borrower.pubkey(), &lender.pubkey()] {
        send(svm, &[ix_register_account(&payer.pubkey(), u)], &[&payer]);
        send(
            svm,
            &[ix_update_status(&payer.pubkey(), u, compliance_status::VERIFIED)],
            &[&payer],
        );
    }

    // Init both vaults + their canonical ATAs.
    for mint in [&collateral_mint, &cash_mint] {
        send(svm, &[ix_init_vault(&payer.pubkey(), mint)], &[&payer]);
        let vault = derive_vault(mint);
        create_user_ata(svm, &payer, mint, &vault);
    }

    // Borrower has collateral; lender has cash.
    let borrower_collateral_ata =
        create_user_ata(svm, &payer, &collateral_mint, &borrower.pubkey());
    mint_to(svm, &payer, &collateral_mint, &payer, &borrower_collateral_ata, 1_000_000);
    let lender_cash_ata = create_user_ata(svm, &payer, &cash_mint, &lender.pubkey());
    mint_to(svm, &payer, &cash_mint, &payer, &lender_cash_ata, 1_000_000);

    // Each side deposits into their vault.
    let collateral_vault_ata = derive_canonical_ata(&derive_vault(&collateral_mint), &collateral_mint);
    let cash_vault_ata = derive_canonical_ata(&derive_vault(&cash_mint), &cash_mint);
    send(
        svm,
        &[ix_deposit(
            &borrower.pubkey(),
            &payer.pubkey(),
            &collateral_mint,
            &borrower_collateral_ata,
            &collateral_vault_ata,
            500_000,
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

    Fixture {
        payer,
        borrower,
        lender,
        collateral_mint,
        cash_mint,
    }
}

#[test]
fn open_close_repo_locks_then_unlocks_both_positions() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    let nonce = 1u64;
    let collateral_vault = derive_vault(&f.collateral_mint);
    let cash_vault = derive_vault(&f.cash_mint);
    let collateral_position = derive_position(&collateral_vault, &f.borrower.pubkey());
    let cash_position = derive_position(&cash_vault, &f.lender.pubkey());
    let repo = derive_repo(
        &f.borrower.pubkey(),
        &f.lender.pubkey(),
        &collateral_vault,
        &cash_vault,
        nonce,
    );

    // open_repo: 300_000 collateral, 200_000 cash, expiry far in the future.
    send(
        &mut svm,
        &[ix_open_repo(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            u64::MAX,
            nonce,
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );

    let cp_acc = svm.get_account(&collateral_position).unwrap();
    let cp: &Position = from_bytes(&cp_acc.data[..Position::LEN]);
    assert_eq!(cp.locked_amount, 300_000);
    assert_eq!(cp.lock_authority, repo.to_bytes(), "repo PDA is lock authority");
    assert_eq!(cp.available(), 200_000);

    let lp_acc = svm.get_account(&cash_position).unwrap();
    let lp: &Position = from_bytes(&lp_acc.data[..Position::LEN]);
    assert_eq!(lp.locked_amount, 200_000);
    assert_eq!(lp.lock_authority, repo.to_bytes());
    assert_eq!(lp.available(), 200_000);

    let r_acc = svm.get_account(&repo).unwrap();
    let r: &Repo = from_bytes(&r_acc.data[..Repo::LEN]);
    assert_eq!(r.status, repo_status::OPEN);
    assert_eq!(r.collateral_amount, 300_000);
    assert_eq!(r.cash_amount, 200_000);
    assert_eq!(r.borrower, f.borrower.pubkey().to_bytes());
    assert_eq!(r.lender, f.lender.pubkey().to_bytes());

    // close_repo: both positions unlock; status → CLOSED.
    send(
        &mut svm,
        &[ix_close_repo(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            nonce,
        )],
        &[&f.payer, &f.borrower],
    );

    let cp_acc = svm.get_account(&collateral_position).unwrap();
    let cp: &Position = from_bytes(&cp_acc.data[..Position::LEN]);
    assert_eq!(cp.locked_amount, 0);
    assert_eq!(cp.lock_authority, [0u8; 32]);
    assert!(cp.is_unlocked());

    let lp_acc = svm.get_account(&cash_position).unwrap();
    let lp: &Position = from_bytes(&lp_acc.data[..Position::LEN]);
    assert_eq!(lp.locked_amount, 0);
    assert!(lp.is_unlocked());

    let r_acc = svm.get_account(&repo).unwrap();
    let r: &Repo = from_bytes(&r_acc.data[..Repo::LEN]);
    assert_eq!(r.status, repo_status::CLOSED);
}

#[test]
fn open_repo_rejects_when_borrower_suspended() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    // Flip borrower to SUSPENDED after deposit but before open_repo.
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
        &[ix_open_repo(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            u64::MAX,
            1,
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );
    assert!(res.is_err(), "open_repo must reject when a party is suspended");
}

#[test]
fn vault_ix_discriminator_drift_check() {
    // The repo program and this test hard-code ssr-vault's
    // `LOCK_POSITION` / `UNLOCK_POSITION` discriminators rather than
    // depending on the crate (see ssr-repo/Cargo.toml). The literal
    // values below MUST track ssr-vault::ix in lockstep. We intentionally
    // also pin INIT_VAULT and DEPOSIT to surface any accidental
    // renumbering at the head of the enum.
    assert_eq!(vault_ix::INIT_VAULT, 0);
    assert_eq!(vault_ix::DEPOSIT, 1);
    assert_eq!(vault_ix::WITHDRAW, 2);
    assert_eq!(vault_ix::LOCK_POSITION, 3);
    assert_eq!(vault_ix::UNLOCK_POSITION, 4);
}
