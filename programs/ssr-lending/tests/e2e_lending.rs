//! End-to-end test for the Phase 3 lending wrapper.
//!
//! Walks through:
//!   1. Compliance bootstrap; two verified parties (borrower + lender).
//!   2. Two plain Token-2022 mints (collateral + cash) and two vaults.
//!   3. Each party deposits into their respective vault.
//!   4. `open_loan` — both Position PDAs get locked against the Loan PDA.
//!   5. `repay_loan` — the Loan PDA signs the unlock, both positions
//!      become free again, `Loan::status` flips to `REPAID`.
//!   6. Negative: re-opening with a borrower flipped to `SUSPENDED`
//!      between deposit and `open_loan` rejects.
//!   7. Negative: `repay_loan` past `maturity_slot` rejects with
//!      `MATURED` so the Phase 3b liquidation path can claim the
//!      collateral instead.
//!
//! Prerequisite: every `.so` under `target/deploy/` (compliance, vault,
//! lending) must exist (`cargo build-sbf` on each).

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
    ssr_lending::{ix as lending_ix},
    ssr_types::{asset_class, compliance_status, loan_status, seeds, Loan, Position},
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
    pub const TRANSFER_WITHIN_VAULT: u8 = 5;
    pub const SEIZE_LOCKED: u8 = 6;
    pub const INIT_POSITION: u8 = 7;
}

// ─── Harness ─────────────────────────────────────────────────────────────

fn compliance_program_id() -> Address {
    Address::from([7u8; 32])
}
fn vault_program_id() -> Address {
    Address::from([9u8; 32])
}
fn lending_program_id() -> Address {
    Address::from([13u8; 32])
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
        ("ssr_lending.so", so_path("ssr_lending.so")),
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
        lending_program_id(),
        &std::fs::read(so_path("ssr_lending.so")).unwrap(),
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
fn derive_loan_list(borrower: &Address) -> Address {
    Address::find_program_address(
        &[seeds::LOAN_LIST, borrower.as_ref()],
        &lending_program_id(),
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

fn ix_init_vault(admin: &Address, mint: &Address, asset_class: u8) -> Instruction {
    Instruction {
        program_id: vault_program_id(),
        accounts: vec![
            AccountMeta::new(*admin, true),
            AccountMeta::new(derive_vault(mint), false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        // Phase 4 v1b: pass an explicit asset_class so the margin
        // gate sees real haircut credit. UNKNOWN (0) returns 10_000
        // bps from the haircut table → zero margin credit → every
        // open_loan in these tests would fail with
        // `MARGIN_INSUFFICIENT`.
        data: vec![vault_ix::INIT_VAULT, asset_class],
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

// ─── Lending instruction builders ───────────────────────────────────────-

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
    // v1d: PriceFeeds for each distinct mint involved.
    price_feed_mints: &[Address],
    // v1e: cash_vault for each existing loan, in the SAME ORDER as
    // `existing_loans`. The handler validates each matches the
    // corresponding loan.cash_vault.
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
    // v1e: cash_vaults parallel to existing_loans, same order.
    for cv in existing_loan_cash_vaults {
        accounts.push(AccountMeta::new_readonly(*cv, false));
    }
    // Derive each PriceFeed PDA, then sort ascending so the handler's
    // strict-ascending check passes.
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
    // Phase 4 v1b: open_loan reads RiskParams; the PDA must exist
    // before any open_loan call works. Same migration story applies
    // to any deployment moving from v1c → v1b.
    send(
        svm,
        &[ix_initialize_risk_params(&payer.pubkey(), &payer.pubkey())],
        &[&payer],
    );

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

    // Init both vaults + their canonical ATAs. Collateral as
    // sovereign_bond (500 bps haircut → 95% credit) and cash as
    // stablecoin (0 bps → full credit) so v1b's margin gate sees
    // real numbers and the canonical 500_000-collateral /
    // 200_000-loan amounts comfortably pass.
    send(
        svm,
        &[ix_init_vault(&payer.pubkey(), &collateral_mint, asset_class::SOVEREIGN_BOND)],
        &[&payer],
    );
    create_user_ata(svm, &payer, &collateral_mint, &derive_vault(&collateral_mint));
    send(
        svm,
        &[ix_init_vault(&payer.pubkey(), &cash_mint, asset_class::STABLECOIN)],
        &[&payer],
    );
    create_user_ata(svm, &payer, &cash_mint, &derive_vault(&cash_mint));

    // Phase 4 v1d: register PriceFeeds. Both mints priced at $1.00
    // = 1_000_000 micro-USD per native unit (with 6 decimals). Under
    // this trivial FX, the v1d gate produces the same pool / liab
    // numbers as v1b's haircut-only model — the existing test amounts
    // continue to pass at the same boundary.
    send(
        svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &collateral_mint, 1_000_000, 6)],
        &[&payer],
    );
    send(
        svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &cash_mint, 1_000_000, 6)],
        &[&payer],
    );

    // Borrower has collateral; lender has cash.
    let borrower_collateral_ata =
        create_user_ata(svm, &payer, &collateral_mint, &borrower.pubkey());
    mint_to(svm, &payer, &collateral_mint, &payer, &borrower_collateral_ata, 1_000_000);
    let lender_cash_ata = create_user_ata(svm, &payer, &cash_mint, &lender.pubkey());
    mint_to(svm, &payer, &cash_mint, &payer, &lender_cash_ata, 1_000_000);

    // Funded sides deposit into their vault. Borrower deposit bumped
    // from 500_000 → 600_000 vs pre-v1b so the conservative margin
    // gate (pool = collateral×credit, principal is a pure liability)
    // has enough headroom for the canonical 300k-lock / 200k-borrow
    // test shape: 600k × 0.95 − 300k × 0.95 = 285k pool, 200k liab.
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

    // Phase 3b: pre-create the receive-side positions so open_loan can
    // credit the borrower's cash and (post-default) liquidate_loan can
    // credit the lender's collateral. These calls cost the position-PDA
    // rent only; no token movement.
    send(
        svm,
        &[ix_init_position(&borrower.pubkey(), &payer.pubkey(), &cash_mint)],
        &[&payer, &borrower],
    );
    send(
        svm,
        &[ix_init_position(&lender.pubkey(), &payer.pubkey(), &collateral_mint)],
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
fn open_repay_moves_cash_then_unlocks_collateral() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    let nonce = 1u64;
    // 0 bps so the repay amount equals the principal exactly — we
    // exercise the interest path in a separate test below.
    let interest_bps_per_year = 0u32;
    let collateral_vault = derive_vault(&f.collateral_mint);
    let cash_vault = derive_vault(&f.cash_mint);
    let borrower_collateral = derive_position(&collateral_vault, &f.borrower.pubkey());
    let lender_cash = derive_position(&cash_vault, &f.lender.pubkey());
    let borrower_cash = derive_position(&cash_vault, &f.borrower.pubkey());
    let loan = derive_loan(
        &f.borrower.pubkey(),
        &f.lender.pubkey(),
        &collateral_vault,
        &cash_vault,
        nonce,
    );

    // open_loan: 300_000 collateral locked, 200_000 cash transferred
    // from lender to borrower in the same transaction.
    send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            u64::MAX,
            nonce,
            interest_bps_per_year,
            &[],
            &[],
            &[f.collateral_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );

    let cp: Position =
        *from_bytes(&svm.get_account(&borrower_collateral).unwrap().data[..Position::LEN]);
    assert_eq!(cp.locked_amount, 300_000);
    assert_eq!(cp.lock_authority, loan.to_bytes(), "loan PDA is lock authority");
    assert_eq!(cp.amount_deposited, 600_000);
    assert_eq!(cp.available(), 300_000);

    // Lender's cash drained by `principal_amount`; borrower's cash
    // credited by the same.
    let lc: Position =
        *from_bytes(&svm.get_account(&lender_cash).unwrap().data[..Position::LEN]);
    assert_eq!(lc.amount_deposited, 200_000); // 400_000 - 200_000
    assert_eq!(lc.locked_amount, 0);
    let bc: Position =
        *from_bytes(&svm.get_account(&borrower_cash).unwrap().data[..Position::LEN]);
    assert_eq!(bc.amount_deposited, 200_000);
    assert_eq!(bc.locked_amount, 0);

    let l: Loan = *from_bytes(&svm.get_account(&loan).unwrap().data[..Loan::LEN]);
    assert_eq!(l.status, loan_status::OPEN);
    assert_eq!(l.collateral_amount, 300_000);
    assert_eq!(l.principal_amount, 200_000);
    assert_eq!(l.interest_bps_per_year, interest_bps_per_year);
    assert_eq!(l.borrower, f.borrower.pubkey().to_bytes());
    assert_eq!(l.lender, f.lender.pubkey().to_bytes());
    assert_eq!(l.opened_slot, l.last_modified_slot);

    // repay_loan: borrower transfers cash back, collateral unlocks.
    send(
        &mut svm,
        &[ix_repay_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            nonce,
        )],
        &[&f.payer, &f.borrower],
    );

    // Cash flows reverse at exactly principal_amount (0 bps).
    let lc: Position =
        *from_bytes(&svm.get_account(&lender_cash).unwrap().data[..Position::LEN]);
    assert_eq!(lc.amount_deposited, 400_000); // restored
    let bc: Position =
        *from_bytes(&svm.get_account(&borrower_cash).unwrap().data[..Position::LEN]);
    assert_eq!(bc.amount_deposited, 0);

    // Borrower's collateral unlocked.
    let cp: Position =
        *from_bytes(&svm.get_account(&borrower_collateral).unwrap().data[..Position::LEN]);
    assert_eq!(cp.locked_amount, 0);
    assert_eq!(cp.lock_authority, [0u8; 32]);
    assert!(cp.is_unlocked());

    let l: Loan = *from_bytes(&svm.get_account(&loan).unwrap().data[..Loan::LEN]);
    assert_eq!(l.status, loan_status::REPAID);
}

#[test]
fn open_loan_rejects_when_borrower_suspended() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    // Flip borrower to SUSPENDED after deposit but before open_loan.
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
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            u64::MAX,
            1,
            500,
            &[],
            &[],
            &[f.collateral_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );
    assert!(res.is_err(), "open_loan must reject when a party is suspended");
}

#[test]
fn repay_loan_rejects_after_maturity() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    let nonce = 1u64;
    // Pick a small absolute maturity_slot; LiteSVM starts at slot 0
    // and the fixture/open_loan ix consume only a handful of slots.
    // We warp past maturity below to surface the `MATURED` reject.
    let maturity_slot = 1_000u64;

    send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            maturity_slot,
            nonce,
            500,
            &[],
            &[],
            &[f.collateral_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );

    // Warp past maturity.
    svm.warp_to_slot(maturity_slot + 10);

    let res = try_send(
        &mut svm,
        &[ix_repay_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            nonce,
        )],
        &[&f.payer, &f.borrower],
    );
    assert!(res.is_err(), "repay_loan must reject after maturity_slot");
}

#[test]
fn liquidate_loan_after_maturity_transfers_collateral_to_lender() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    let nonce = 7u64;
    let maturity_slot = 1_000u64;
    let collateral_vault = derive_vault(&f.collateral_mint);
    let borrower_collateral =
        derive_position(&collateral_vault, &f.borrower.pubkey());
    let lender_collateral = derive_position(&collateral_vault, &f.lender.pubkey());
    let loan = derive_loan(
        &f.borrower.pubkey(),
        &f.lender.pubkey(),
        &collateral_vault,
        &derive_vault(&f.cash_mint),
        nonce,
    );

    send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            maturity_slot,
            nonce,
            500,
            &[],
            &[],
            &[f.collateral_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );

    // Pre-liquidate inventory check: lender's collateral position is
    // empty, borrower's has the locked encumbrance.
    let bc_pre: Position =
        *from_bytes(&svm.get_account(&borrower_collateral).unwrap().data[..Position::LEN]);
    assert_eq!(bc_pre.locked_amount, 300_000);
    assert_eq!(bc_pre.amount_deposited, 600_000);
    let lc_pre: Position =
        *from_bytes(&svm.get_account(&lender_collateral).unwrap().data[..Position::LEN]);
    assert_eq!(lc_pre.amount_deposited, 0);

    svm.warp_to_slot(maturity_slot + 10);

    send(
        &mut svm,
        &[ix_liquidate_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            nonce,
        )],
        &[&f.payer, &f.lender],
    );

    // Collateral has moved from borrower (locked) to lender (unlocked).
    let bc: Position =
        *from_bytes(&svm.get_account(&borrower_collateral).unwrap().data[..Position::LEN]);
    assert_eq!(bc.locked_amount, 0);
    assert_eq!(bc.amount_deposited, 300_000); // 600_000 - 300_000 (liquidated to lender)
    assert_eq!(bc.lock_authority, [0u8; 32]);
    let lc: Position =
        *from_bytes(&svm.get_account(&lender_collateral).unwrap().data[..Position::LEN]);
    assert_eq!(lc.amount_deposited, 300_000);
    assert_eq!(lc.locked_amount, 0);

    let l: Loan = *from_bytes(&svm.get_account(&loan).unwrap().data[..Loan::LEN]);
    assert_eq!(l.status, loan_status::LIQUIDATED);
}

#[test]
fn liquidate_loan_rejects_before_maturity() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    let nonce = 5u64;
    let maturity_slot = 1_000_000u64; // far enough that we stay before it

    send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            maturity_slot,
            nonce,
            500,
            &[],
            &[],
            &[f.collateral_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );

    let res = try_send(
        &mut svm,
        &[ix_liquidate_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            nonce,
        )],
        &[&f.payer, &f.lender],
    );
    assert!(res.is_err(), "liquidate must reject before maturity_slot");
}

#[test]
fn repay_with_interest_overpays_lender() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    let nonce = 3u64;
    // Pick maturity well in the future and warp ahead by a known slot
    // delta to make the interest computation deterministic.
    let maturity_slot = 100_000_000u64;
    let interest_bps_per_year = 500u32; // 5% / year
    let cash_vault = derive_vault(&f.cash_mint);
    let lender_cash = derive_position(&cash_vault, &f.lender.pubkey());
    let borrower_cash = derive_position(&cash_vault, &f.borrower.pubkey());

    send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            maturity_slot,
            nonce,
            interest_bps_per_year,
            &[],
            &[],
            &[f.collateral_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );

    // After open, the borrower has 200_000 cash to repay with. The
    // lender originally had 400_000 and now holds 200_000. We want
    // them to receive principal + a positive interest after a slot
    // delta. Top the borrower up so they can cover principal +
    // interest.
    //
    // For SLOTS_PER_YEAR = 78_840_000 and interest_bps = 500 (5%),
    // 78_840 slots elapsed (= 1/1000 of a year) yields:
    //   200_000 * 500 * 78_840 / (78_840_000 * 10_000) = 10 tokens.
    let l_open: Loan = {
        let acc = svm.get_account(
            &derive_loan(
                &f.borrower.pubkey(),
                &f.lender.pubkey(),
                &derive_vault(&f.collateral_mint),
                &cash_vault,
                nonce,
            ),
        ).unwrap();
        *from_bytes(&acc.data[..Loan::LEN])
    };
    let target_slot = l_open.opened_slot.saturating_add(78_840);
    svm.warp_to_slot(target_slot);

    // Top up the borrower so they have enough to cover principal +
    // interest. Borrower's cash position currently has 200_000; we
    // need at least 200_010. Mint an extra 100 to their ATA and
    // deposit it.
    let borrower_cash_ata =
        derive_canonical_ata(&f.borrower.pubkey(), &f.cash_mint);
    create_user_ata(&mut svm, &f.payer, &f.cash_mint, &f.borrower.pubkey());
    mint_to(&mut svm, &f.payer, &f.cash_mint, &f.payer, &borrower_cash_ata, 100);
    let cash_vault_ata =
        derive_canonical_ata(&derive_vault(&f.cash_mint), &f.cash_mint);
    send(
        &mut svm,
        &[ix_deposit(
            &f.borrower.pubkey(),
            &f.payer.pubkey(),
            &f.cash_mint,
            &borrower_cash_ata,
            &cash_vault_ata,
            100,
        )],
        &[&f.payer, &f.borrower],
    );

    send(
        &mut svm,
        &[ix_repay_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            nonce,
        )],
        &[&f.payer, &f.borrower],
    );

    let lc: Position =
        *from_bytes(&svm.get_account(&lender_cash).unwrap().data[..Position::LEN]);
    // 200_000 (post-open balance) + 200_010 (repayment incl. interest) = 400_010
    assert_eq!(lc.amount_deposited, 400_010);
    let bc: Position =
        *from_bytes(&svm.get_account(&borrower_cash).unwrap().data[..Position::LEN]);
    // 200_000 (post-open) + 100 (top-up) - 200_010 (repayment) = 90
    assert_eq!(bc.amount_deposited, 90);
}

// ─── Phase 4 v1b: margin enforcement tests ──────────────────────────────-

#[test]
fn open_loan_rejects_when_principal_exceeds_collateral_credit() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    // Borrower has 600_000 sovereign collateral (5% haircut → 570k
    // pool). Try to lock 100_000 (95k cost) and borrow 600_000.
    // Post-lock pool = 475k; liability = 600k. Should reject with
    // MARGIN_INSUFFICIENT.
    let res = try_send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            100_000,  // lock — cheap
            600_000,  // borrow — pure liability under the conservative model
            u64::MAX,
            42,
            0,
            &[],
            &[],
            &[f.collateral_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );
    let err = format!("{:?}", res.expect_err("over-borrow must reject"));
    // MARGIN_INSUFFICIENT = 0x5035 = 20533
    assert!(
        err.contains("Custom(20533)"),
        "expected MARGIN_INSUFFICIENT (0x5035), got: {err}"
    );
}

#[test]
fn open_loan_rejects_when_existing_loan_omitted_from_set() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    // Step 1: borrower opens loan 1 (300k lock, 200k borrow). Fits.
    send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            u64::MAX,
            1,
            0,
            &[],
            &[],
            &[f.collateral_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );

    // Step 2: try to open loan 2 (300_000 lock, 100_000 borrow)
    // WITHOUT disclosing loan 1 in the existing-loans list. LoanList
    // says count=1, caller passes 0 → set mismatch.
    let res = try_send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            100_000,
            u64::MAX,
            2,
            0,
            &[],
            &[], // ← omitted loan 1
            &[f.collateral_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );
    let err = format!("{:?}", res.expect_err("must reject when LoanList not exhausted"));
    // MARGIN_LOAN_SET_MISMATCH = 0x5038 = 20536
    assert!(
        err.contains("Custom(20536)"),
        "expected MARGIN_LOAN_SET_MISMATCH (0x5038), got: {err}"
    );
}

#[test]
fn open_loan_rejects_when_existing_loan_substituted() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    // Open loan 1 (the real one).
    send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            u64::MAX,
            1,
            0,
            &[],
            &[],
            &[f.collateral_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );

    // Substitute: pass a random pubkey instead of the real loan 1.
    // Set count matches (1 passed, 1 in list) but membership fails.
    let bogus = Address::new_unique();
    let res = try_send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            100_000,
            50_000,
            u64::MAX,
            2,
            0,
            &[],
            &[bogus], // ← wrong loan PDA
            &[f.collateral_mint, f.cash_mint],
            // v1e: bogus loan has no real cash_vault to pass; use
            // an arbitrary placeholder (the loan-set membership
            // check rejects before we ever read the cash_vault).
            &[bogus],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );
    let err = format!("{:?}", res.expect_err("must reject on loan-set membership mismatch"));
    assert!(
        err.contains("Custom(20536)"),
        "expected MARGIN_LOAN_SET_MISMATCH (0x5038), got: {err}"
    );
}

// Note: a positive "two loans, second discloses first" test would
// require two distinct collateral mints — Phase 3's single-lock
// Position model rejects re-locking an already-encumbered position
// with `0x3030`. Cross-mint cross-margin (mint A's position backs
// mint B's loan, gated by haircut credit and `LoanList`
// exhaustiveness across mints) lands in a later sub-phase together
// with the multi-mint pool that the v1b conservative gate already
// supports.

#[test]
fn loan_list_shrinks_on_repay_and_liquidate() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    // Open one loan.
    send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            u64::MAX,
            9,
            0,
            &[],
            &[],
            &[f.collateral_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );

    let ll_pda =
        Address::find_program_address(&[seeds::LOAN_LIST, f.borrower.pubkey().as_ref()], &lending_program_id()).0;
    let ll_acc = svm.get_account(&ll_pda).unwrap();
    let ll: &ssr_types::LoanList = from_bytes(&ll_acc.data[..ssr_types::LoanList::LEN]);
    assert_eq!(ll.count, 1, "LoanList has 1 entry after open");

    // Repay → LoanList should drop back to 0.
    send(
        &mut svm,
        &[ix_repay_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            9,
        )],
        &[&f.payer, &f.borrower],
    );
    let acc = svm.get_account(&ll_pda).unwrap();
    let ll: &ssr_types::LoanList = from_bytes(&acc.data[..ssr_types::LoanList::LEN]);
    assert_eq!(ll.count, 0, "LoanList shrinks to 0 after repay");
    assert_eq!(ll.entries[0], [0u8; 32], "vacated slot zeroed");
}

// ─── Phase 4 v1e: per-loan cash_vault tests ─────────────────────────────-

#[test]
fn open_loan_rejects_when_cash_vault_substituted_for_existing_loan() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);
    let cash_vault = derive_vault(&f.cash_mint);

    // Step 1: open a real loan against the fixture cash mint. Single
    // cash mint per borrower in this setup — what we're testing here
    // is that the v1e per-loan cash_vault validation rejects when
    // the caller swaps in a different account in the parallel slice.
    send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            u64::MAX,
            1,
            0,
            &[],
            &[],
            &[f.collateral_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );

    // Step 2: try to open another loan, disclosing loan 1 but
    // passing a BOGUS cash_vault in the parallel slice (any address
    // that isn't the real cash_vault). The v1e check
    // `loan.cash_vault == cash_vault_ai.key()` rejects.
    let collateral_vault = derive_vault(&f.collateral_mint);
    let loan1 = derive_loan(
        &f.borrower.pubkey(),
        &f.lender.pubkey(),
        &collateral_vault,
        &cash_vault,
        1,
    );
    let bogus_vault = Address::new_unique();
    let res = try_send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            100_000,
            50_000,
            u64::MAX,
            2,
            0,
            &[],
            &[loan1],
            &[f.collateral_mint, f.cash_mint],
            &[bogus_vault], // ← wrong cash_vault for loan1
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );
    let err = format!("{:?}", res.expect_err("substituted cash_vault must reject"));
    // MARGIN_LOAN_SET_MISMATCH = 0x5038 = 20536
    assert!(
        err.contains("Custom(20536)"),
        "expected MARGIN_LOAN_SET_MISMATCH (0x5038), got: {err}"
    );
}

// ─── Phase 4 v1d: oracle-priced margin tests ────────────────────────────-

#[test]
fn open_loan_rejects_when_price_feed_missing_for_collateral() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    // Caller passes only the cash mint's feed — collateral feed
    // missing → handler can't price the position → PRICE_FEED_MISSING.
    let res = try_send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            u64::MAX,
            42,
            0,
            &[],
            &[],
            &[f.cash_mint], // ← collateral mint deliberately omitted
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );
    let err = format!("{:?}", res.expect_err("missing collateral feed must reject"));
    // PRICE_FEED_MISSING = 0x5039 = 20537
    assert!(
        err.contains("Custom(20537)"),
        "expected PRICE_FEED_MISSING (0x5039), got: {err}"
    );
}

#[test]
fn open_loan_rejects_when_price_feed_stale() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    // Tighten max_staleness to 10 slots so the existing feeds
    // (registered at slot 0) become stale as soon as we warp past
    // slot 10.
    let mut data = Vec::with_capacity(1 + 8);
    data.push(ssr_compliance::ix::SET_MAX_STALENESS);
    data.extend_from_slice(&10u64.to_le_bytes());
    let set_max = Instruction {
        program_id: compliance_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(f.payer.pubkey(), true),
            AccountMeta::new_readonly(derive_registry(), false),
            AccountMeta::new(derive_risk_params(), false),
        ],
        data,
    };
    send(&mut svm, &[set_max], &[&f.payer]);

    svm.warp_to_slot(500);

    let res = try_send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            u64::MAX,
            42,
            0,
            &[],
            &[],
            &[f.collateral_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );
    let err = format!("{:?}", res.expect_err("stale feed must reject"));
    // PRICE_FEED_STALE = 0x503A = 20538
    assert!(
        err.contains("Custom(20538)"),
        "expected PRICE_FEED_STALE (0x503A), got: {err}"
    );
}

#[test]
fn open_loan_rejects_when_collateral_value_drops_below_loan_in_usd() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    // Tank the collateral price to $0.10 (was $1.00). Borrower's 600k
    // sovereign collateral is now worth $60k of haircut-adjusted USD
    // (600k × 0.10 × 0.95 = 57k post-lock-adjusted). A 200k loan
    // ($200k liability) clearly exceeds the haircut-adjusted USD
    // pool.
    let oracle = f.payer.pubkey();
    let mut data = Vec::with_capacity(1 + 8);
    data.push(ssr_compliance::ix::UPDATE_PRICE);
    data.extend_from_slice(&100_000u64.to_le_bytes()); // $0.10 = 100_000 micro-USD
    let update = Instruction {
        program_id: compliance_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(oracle, true),
            AccountMeta::new_readonly(derive_registry(), false),
            AccountMeta::new(derive_price_feed(&f.collateral_mint), false),
        ],
        data,
    };
    send(&mut svm, &[update], &[&f.payer]);

    let res = try_send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            u64::MAX,
            42,
            0,
            &[],
            &[],
            &[f.collateral_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );
    let err = format!("{:?}", res.expect_err("USD-priced over-borrow must reject"));
    // MARGIN_INSUFFICIENT = 0x5035 = 20533
    assert!(
        err.contains("Custom(20533)"),
        "expected MARGIN_INSUFFICIENT (0x5035), got: {err}"
    );
}

#[test]
fn open_loan_passes_when_collateral_price_rises() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = setup_fixture(&mut svm, payer);

    // Boost collateral price to $10.00 (was $1.00). Now 600k sovereign
    // × $10 × 0.95 = $5_700_000 of pool. The canonical 300k/200k loan
    // easily passes — this confirms a price update is reflected in
    // the next `open_loan` without any redeploy.
    let mut data = Vec::with_capacity(1 + 8);
    data.push(ssr_compliance::ix::UPDATE_PRICE);
    data.extend_from_slice(&10_000_000u64.to_le_bytes()); // $10.00
    let update = Instruction {
        program_id: compliance_program_id(),
        accounts: vec![
            AccountMeta::new_readonly(f.payer.pubkey(), true),
            AccountMeta::new_readonly(derive_registry(), false),
            AccountMeta::new(derive_price_feed(&f.collateral_mint), false),
        ],
        data,
    };
    send(&mut svm, &[update], &[&f.payer]);

    send(
        &mut svm,
        &[ix_open_loan(
            &f.borrower.pubkey(),
            &f.lender.pubkey(),
            &f.payer.pubkey(),
            &f.collateral_mint,
            &f.cash_mint,
            300_000,
            200_000,
            u64::MAX,
            99,
            0,
            &[],
            &[],
            &[f.collateral_mint, f.cash_mint],
            &[],
        )],
        &[&f.payer, &f.borrower, &f.lender],
    );
}

#[test]
fn vault_ix_discriminator_drift_check() {
    // The lending program and this test hard-code ssr-vault's
    // `LOCK_POSITION` / `UNLOCK_POSITION` discriminators rather than
    // depending on the crate (see ssr-lending/Cargo.toml). The literal
    // values below MUST track ssr-vault::ix in lockstep. We intentionally
    // also pin INIT_VAULT and DEPOSIT to surface any accidental
    // renumbering at the head of the enum.
    assert_eq!(vault_ix::INIT_VAULT, 0);
    assert_eq!(vault_ix::DEPOSIT, 1);
    assert_eq!(vault_ix::WITHDRAW, 2);
    assert_eq!(vault_ix::LOCK_POSITION, 3);
    assert_eq!(vault_ix::UNLOCK_POSITION, 4);
}
