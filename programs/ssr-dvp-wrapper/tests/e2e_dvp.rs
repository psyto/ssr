//! End-to-end Token-2022 + ssr-compliance + ssr-dvp-wrapper + SPC
//! `dvp-swap-program` integration.
//!
//! Demonstrates Model C (hybrid) from `docs/spc-integration.md`:
//!   * SSR-tagged Token-2022 mints carry **no** TransferHook.
//!   * `ssr-compliance` runs as the on-chain registry of who is
//!     `VERIFIED`.
//!   * `ssr-dvp-wrapper` sits in front of SPC's atomic-swap primitive
//!     as the `settlement_authority`. It verifies both parties before
//!     CPI-ing `SettleDvp`.
//!
//! The test loads three BPF programs in-process via LiteSVM and walks
//! through a real atomic asset↔cash trade.
//!
//! Prerequisites:
//!   * `cargo build-sbf` produces `target/deploy/ssr_compliance.so` and
//!     `target/deploy/ssr_dvp_wrapper.so`.
//!   * `tests/fixtures/dvp_swap_program.so` is the SPC artifact (built
//!     once from `/Users/hiroyusai/src/spc-reference/dvp-swap-program/`).
//!
//! Tests skip with a clear message if any artifact is missing.

use {
    litesvm::LiteSVM,
    solana_address::Address,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_message::Message,
    solana_program_pack::Pack,
    solana_signer::Signer,
    solana_transaction::Transaction,
    spl_token_2022::state::{Account as TokenAccount, Mint},
    ssr_dvp_wrapper::{
        AUTHORITY_SEED, SPC_DVP_PROGRAM_ID, SPC_IX_SETTLE_DVP, err as wrapper_err, ix as wrapper_ix,
    },
    ssr_types::{compliance_status, seeds},
    std::path::PathBuf,
};

// ─── Common harness ──────────────────────────────────────────────────────

fn compliance_program_id() -> Address {
    Address::from([7u8; 32])
}
fn wrapper_program_id() -> Address {
    Address::from([8u8; 32])
}
fn spc_dvp_program_id() -> Address {
    Address::from(SPC_DVP_PROGRAM_ID)
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
fn compliance_so() -> PathBuf {
    manifest_dir().join("../../target/deploy/ssr_compliance.so")
}
fn wrapper_so() -> PathBuf {
    manifest_dir().join("../../target/deploy/ssr_dvp_wrapper.so")
}
fn spc_dvp_so() -> PathBuf {
    manifest_dir().join("../../tests/fixtures/dvp_swap_program.so")
}

fn setup() -> Option<(LiteSVM, Keypair)> {
    for (label, path) in [
        ("ssr_compliance.so", compliance_so()),
        ("ssr_dvp_wrapper.so", wrapper_so()),
        ("dvp_swap_program.so", spc_dvp_so()),
    ] {
        if !path.exists() {
            eprintln!(
                "SKIP: {label} not found at {}.\n\
                 - For *.so under target/deploy/, run \
                 `cargo build-sbf --manifest-path programs/<crate>/Cargo.toml` first.\n\
                 - For dvp_swap_program.so under tests/fixtures/, build it from \
                 /Users/hiroyusai/src/spc-reference/dvp-swap-program/program/ and copy.",
                path.display()
            );
            return None;
        }
    }

    let mut svm = LiteSVM::new();
    svm.add_program(
        compliance_program_id(),
        &std::fs::read(compliance_so()).unwrap(),
    )
    .unwrap();
    svm.add_program(
        wrapper_program_id(),
        &std::fs::read(wrapper_so()).unwrap(),
    )
    .unwrap();
    svm.add_program(
        spc_dvp_program_id(),
        &std::fs::read(spc_dvp_so()).unwrap(),
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

// ─── Token-2022 setup helpers (no TransferHook this time) ────────────────

fn create_plain_mint(
    svm: &mut LiteSVM,
    payer: &Keypair,
    mint_kp: &Keypair,
    mint_authority: &Address,
    decimals: u8,
) {
    let space = Mint::LEN;
    let lamports = svm.minimum_balance_for_rent_exemption(space);
    let create_acc = solana_system_interface::instruction::create_account(
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
    send(svm, &[create_acc, init], &[payer, mint_kp]);
}

fn create_token_account(
    svm: &mut LiteSVM,
    payer: &Keypair,
    account_kp: &Keypair,
    mint: &Address,
    owner: &Address,
) {
    let space = TokenAccount::LEN;
    let lamports = svm.minimum_balance_for_rent_exemption(space);
    let create = solana_system_interface::instruction::create_account(
        &payer.pubkey(),
        &account_kp.pubkey(),
        lamports,
        space as u64,
        &spl_token_2022::ID,
    );
    let init = spl_token_2022::instruction::initialize_account3(
        &spl_token_2022::ID,
        &account_kp.pubkey(),
        mint,
        owner,
    )
    .unwrap();
    send(svm, &[create, init], &[payer, account_kp]);
}

fn mint_to(
    svm: &mut LiteSVM,
    payer: &Keypair,
    mint: &Address,
    mint_authority: &Keypair,
    dest: &Address,
    amount: u64,
) {
    let ix = spl_token_2022::instruction::mint_to(
        &spl_token_2022::ID,
        mint,
        dest,
        &mint_authority.pubkey(),
        &[],
        amount,
    )
    .unwrap();
    send(svm, &[ix], &[payer, mint_authority]);
}

fn balance(svm: &LiteSVM, token_account: &Address) -> u64 {
    let acc = svm.get_account(token_account).unwrap();
    TokenAccount::unpack(&acc.data[..TokenAccount::LEN])
        .unwrap()
        .amount
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

// ─── SPC DvP CreateDvp instruction builder ───────────────────────────────

const SPC_IX_CREATE_DVP: u8 = 0;

/// SPC SwapDvp PDA seeds:
///   [b"dvp", settlement_authority, user_a, user_b, mint_a, mint_b, nonce_le_bytes]
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

// SPL Associated Token Account program ID.
const ATA_PROGRAM_ID: Address = Address::new_from_array(
    pinocchio_pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
);

/// Derive the canonical associated-token-account address for an owner
/// + Token-2022 mint pair. SPC's `SettleDvp` re-derives the same
/// address for every leg/refund account and rejects with `InvalidSeeds`
/// if anything passed in differs — so all six ATAs must be canonical.
fn derive_canonical_ata(owner: &Address, mint: &Address) -> Address {
    Address::find_program_address(
        &[
            owner.as_ref(),
            spl_token_2022::ID.to_bytes().as_ref(),
            mint.as_ref(),
        ],
        &ATA_PROGRAM_ID,
    )
    .0
}

/// Build a `CreateAssociatedTokenAccountIdempotent` instruction directly
/// (data byte = 1) so we don't drag in another Pubkey-vs-Address
/// ecosystem mismatch from the SPL helper crate.
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
            AccountMeta::new_readonly(spl_token_2022::ID.to_bytes().into(), false),
        ],
        data: vec![1u8], // CreateIdempotent discriminator
    };
    send(svm, &[ix], &[payer]);
    ata
}

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
            AccountMeta::new_readonly(spl_token_2022::ID.to_bytes().into(), false),
            AccountMeta::new_readonly(spl_token_2022::ID.to_bytes().into(), false),
            AccountMeta::new_readonly(ATA_PROGRAM_ID, false),
        ],
        data,
    }
}

// ─── ssr-dvp-wrapper compliant_settle_dvp instruction builder ─────────────

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
            // [0] wrapper_authority_pda (signer via PDA, writable)
            AccountMeta::new(*wrapper_authority, false),
            // [1] ssr_compliance_program_id (owner reference)
            AccountMeta::new_readonly(compliance_program_id(), false),
            // [2] spc_dvp_program (executable, target of CPI)
            AccountMeta::new_readonly(spc_dvp_program_id(), false),
            // [3..5] record PDAs
            AccountMeta::new_readonly(*user_a_record, false),
            AccountMeta::new_readonly(*user_b_record, false),
            // [5..16] SPC SettleDvp accounts[1..12]
            AccountMeta::new(*swap_dvp, false),
            AccountMeta::new_readonly(*mint_a, false),
            AccountMeta::new_readonly(*mint_b, false),
            AccountMeta::new(*dvp_ata_a, false),
            AccountMeta::new(*dvp_ata_b, false),
            AccountMeta::new(*user_a_ata_b, false),
            AccountMeta::new(*user_b_ata_a, false),
            AccountMeta::new(*user_a_ata_a, false),
            AccountMeta::new(*user_b_ata_b, false),
            AccountMeta::new_readonly(spl_token_2022::ID.to_bytes().into(), false),
            AccountMeta::new_readonly(spl_token_2022::ID.to_bytes().into(), false),
        ],
        data: vec![wrapper_ix::COMPLIANT_SETTLE_DVP, leg_a_extras_count],
    }
}

// ─── Test fixture builder ───────────────────────────────────────────────-

struct DvpFixture {
    payer: Keypair,
    user_a: Keypair,
    user_b: Keypair,
    mint_a: Address,
    mint_b: Address,
    user_a_ata_a: Address,
    user_a_ata_b: Address,
    user_b_ata_a: Address,
    user_b_ata_b: Address,
    dvp_ata_a: Address,
    dvp_ata_b: Address,
    swap_dvp: Address,
    user_a_record: Address,
    user_b_record: Address,
    wrapper_authority: Address,
}

/// Build the full happy-path fixture up to the point where `compliant_settle_dvp`
/// would be called. Returns everything needed to invoke / assert.
fn build_dvp_fixture(svm: &mut LiteSVM, payer: Keypair) -> DvpFixture {
    // 1. Bootstrap compliance registry.
    send(svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    // 2. Create two plain Token-2022 mints (no TransferHook — Model C).
    let mint_a_kp = Keypair::new();
    let mint_b_kp = Keypair::new();
    create_plain_mint(svm, &payer, &mint_a_kp, &payer.pubkey(), 6);
    create_plain_mint(svm, &payer, &mint_b_kp, &payer.pubkey(), 6);
    let mint_a = mint_a_kp.pubkey();
    let mint_b = mint_b_kp.pubkey();

    // 3. Two participants and their four token accounts.
    let user_a = Keypair::new();
    let user_b = Keypair::new();
    svm.airdrop(&user_a.pubkey(), 1_000_000_000).unwrap();
    svm.airdrop(&user_b.pubkey(), 1_000_000_000).unwrap();

    let user_a_ata_a = create_user_ata(svm, &payer, &mint_a, &user_a.pubkey());
    let user_a_ata_b = create_user_ata(svm, &payer, &mint_b, &user_a.pubkey());
    let user_b_ata_a = create_user_ata(svm, &payer, &mint_a, &user_b.pubkey());
    let user_b_ata_b = create_user_ata(svm, &payer, &mint_b, &user_b.pubkey());

    // 4. Register and (in the happy path) verify both participants.
    for u in [&user_a.pubkey(), &user_b.pubkey()] {
        send(svm, &[ix_register_account(&payer.pubkey(), u)], &[&payer]);
        send(
            svm,
            &[ix_update_status(&payer.pubkey(), u, compliance_status::VERIFIED)],
            &[&payer],
        );
    }

    // 5. Mint asset to user_a and cash to user_b.
    mint_to(svm, &payer, &mint_a, &payer, &user_a_ata_a, 1_000_000);
    mint_to(svm, &payer, &mint_b, &payer, &user_b_ata_b, 1_000_000);

    // 6. CreateDvp on SPC with our wrapper PDA as settlement_authority.
    let wrapper_authority = derive_wrapper_authority();
    let nonce = 1u64;
    let expiry = 9_999_999_999i64;
    let swap_dvp = derive_swap_dvp(
        &wrapper_authority,
        &user_a.pubkey(),
        &user_b.pubkey(),
        &mint_a,
        &mint_b,
        nonce,
    );
    let dvp_ata_a = derive_canonical_ata(&swap_dvp, &mint_a);
    let dvp_ata_b = derive_canonical_ata(&swap_dvp, &mint_b);

    send(
        svm,
        &[ix_spc_create_dvp(
            &payer.pubkey(),
            &swap_dvp,
            &wrapper_authority,
            &mint_a,
            &mint_b,
            &dvp_ata_a,
            &dvp_ata_b,
            &user_a.pubkey(),
            &user_b.pubkey(),
            500_000,
            500_000,
            expiry,
            nonce,
        )],
        &[&payer],
    );

    // 7. Each party funds their leg via raw SPL Transfer to the escrow ATAs.
    transfer_spl(svm, &payer, &user_a_ata_a, &dvp_ata_a, &user_a, 500_000);
    transfer_spl(svm, &payer, &user_b_ata_b, &dvp_ata_b, &user_b, 500_000);

    let user_a_record = derive_record(&user_a.pubkey());
    let user_b_record = derive_record(&user_b.pubkey());

    DvpFixture {
        payer,
        user_a,
        user_b,
        mint_a,
        mint_b,
        user_a_ata_a,
        user_a_ata_b,
        user_b_ata_a,
        user_b_ata_b,
        dvp_ata_a,
        dvp_ata_b,
        swap_dvp,
        user_a_record,
        user_b_record,
        wrapper_authority,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[test]
fn verified_dvp_settles_atomically_via_wrapper() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = build_dvp_fixture(&mut svm, payer);

    // Sanity: both escrows hold their leg amount.
    assert_eq!(balance(&svm, &f.dvp_ata_a), 500_000);
    assert_eq!(balance(&svm, &f.dvp_ata_b), 500_000);

    // Call the wrapper. leg_a_extras_count = 0 (no TransferHook on these mints).
    let ix = ix_compliant_settle_dvp(
        &f.wrapper_authority,
        &f.swap_dvp,
        &f.mint_a,
        &f.mint_b,
        &f.dvp_ata_a,
        &f.dvp_ata_b,
        &f.user_a_ata_b,
        &f.user_b_ata_a,
        &f.user_a_ata_a,
        &f.user_b_ata_b,
        &f.user_a_record,
        &f.user_b_record,
        0,
    );
    send(&mut svm, &[ix], &[&f.payer]);

    // user_a received the cash leg (mint_b), user_b received the asset leg (mint_a).
    assert_eq!(balance(&svm, &f.user_a_ata_b), 500_000, "user_a got mint_b");
    assert_eq!(balance(&svm, &f.user_b_ata_a), 500_000, "user_b got mint_a");
    // Escrows should be closed; querying the closed account yields None.
    assert!(
        svm.get_account(&f.swap_dvp).is_none() || svm.get_account(&f.swap_dvp).unwrap().lamports == 0,
        "swap_dvp closed"
    );
}

#[test]
fn settle_rejects_when_dest_party_is_suspended() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let f = build_dvp_fixture(&mut svm, payer);

    // Flip user_b's status to SUSPENDED after the DvP was already
    // created and funded — exactly the "compliance pulled a flag
    // post-trade-creation" scenario operators worry about.
    send(
        &mut svm,
        &[ix_update_status(&f.payer.pubkey(), &f.user_b.pubkey(), compliance_status::SUSPENDED)],
        &[&f.payer],
    );

    let ix = ix_compliant_settle_dvp(
        &f.wrapper_authority,
        &f.swap_dvp,
        &f.mint_a,
        &f.mint_b,
        &f.dvp_ata_a,
        &f.dvp_ata_b,
        &f.user_a_ata_b,
        &f.user_b_ata_a,
        &f.user_a_ata_a,
        &f.user_b_ata_b,
        &f.user_a_record,
        &f.user_b_record,
        0,
    );
    let res = try_send(&mut svm, &[ix], &[&f.payer]);
    assert!(res.is_err(), "settle must reject when a party is suspended");

    // Escrows and user balances remain unchanged (funds recoverable).
    assert_eq!(balance(&svm, &f.dvp_ata_a), 500_000);
    assert_eq!(balance(&svm, &f.dvp_ata_b), 500_000);
    assert_eq!(balance(&svm, &f.user_a_ata_b), 0);
    assert_eq!(balance(&svm, &f.user_b_ata_a), 0);
}

// Touch the imports that the negative-test path uses for clarity.
#[allow(dead_code)]
fn _force_use_of_wrapper_constants() {
    let _ = SPC_IX_SETTLE_DVP;
    let _ = wrapper_err::COMPLIANCE_SUSPENDED;
}
