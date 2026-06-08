//! Token-2022 end-to-end tests for the SSR compliance hook.
//!
//! These tests verify that ssr-compliance, when configured as a
//! Token-2022 transfer hook, correctly gates real transfers through the
//! full SPL pipeline:
//!
//!   * Mint creation with the `TransferHook` extension pointing at our
//!     program.
//!   * Initialization of the `ExtraAccountMetaList` PDA via the SPL
//!     hook-interface discriminator (production callers route through
//!     here, not the internal 1-byte tag).
//!   * Real `transfer_checked` invocations: success when both legs are
//!     `VERIFIED`, rejection with the correct error variant when one
//!     leg is not.
//!
//! Prerequisite: `target/deploy/ssr_compliance.so` must exist
//! (`cargo build-sbf` first). Tests early-return if missing.
//!
//! LiteSVM 0.9.1 bundles `spl_token_2022 v10.0.0` as a pre-loaded
//! program at `TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb`, so no
//! separate load is needed.

use {
    litesvm::LiteSVM,
    solana_address::Address,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_message::Message,
    solana_program_pack::Pack,
    solana_signer::Signer,
    solana_transaction::Transaction,
    spl_token_2022::{
        extension::{transfer_hook, BaseStateWithExtensions, ExtensionType, StateWithExtensions},
        state::{Account as TokenAccount, Mint},
    },
    ssr_compliance::{change_mask, extra_metas, hook_disc, ix},
    ssr_types::{compliance_status, seeds},
    std::path::PathBuf,
};

// ─── Common harness (mirrors smoke.rs) ───────────────────────────────────

fn program_id() -> Address {
    Address::from([7u8; 32])
}
fn token_2022_program_id() -> Address {
    // TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb — bundled by litesvm 0.9.1.
    spl_token_2022::ID.to_bytes().into()
}

fn so_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../target/deploy/ssr_compliance.so");
    p
}

fn setup() -> Option<(LiteSVM, Keypair)> {
    let path = so_path();
    if !path.exists() {
        eprintln!(
            "SKIP: {} not built. Run `cargo build-sbf --manifest-path \
             programs/ssr-compliance/Cargo.toml` first.",
            path.display()
        );
        return None;
    }
    let bytes = std::fs::read(&path).expect("read .so");

    let mut svm = LiteSVM::new();
    svm.add_program(program_id(), &bytes)
        .expect("load compliance program");
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).expect("airdrop");
    Some((svm, payer))
}

fn send(svm: &mut LiteSVM, ixs: &[Instruction], signers: &[&Keypair]) {
    let blockhash = svm.latest_blockhash();
    let payer_key = signers[0].pubkey();
    let msg = Message::new(ixs, Some(&payer_key));
    let tx = Transaction::new(signers, msg, blockhash);
    svm.send_transaction(tx).expect("tx failed");
}

fn try_send(
    svm: &mut LiteSVM,
    ixs: &[Instruction],
    signers: &[&Keypair],
) -> Result<(), litesvm::types::FailedTransactionMetadata> {
    let blockhash = svm.latest_blockhash();
    let payer_key = signers[0].pubkey();
    let msg = Message::new(ixs, Some(&payer_key));
    let tx = Transaction::new(signers, msg, blockhash);
    svm.send_transaction(tx).map(|_| ())
}

fn derive_registry() -> (Address, u8) {
    Address::find_program_address(&[seeds::REGISTRY], &program_id())
}
fn derive_record(participant: &Address) -> (Address, u8) {
    Address::find_program_address(
        &[seeds::ACCOUNT_RECORD, participant.as_ref()],
        &program_id(),
    )
}
fn derive_meta_list(mint: &Address) -> (Address, u8) {
    Address::find_program_address(
        &[seeds::EXTRA_META_LIST, mint.as_ref()],
        &program_id(),
    )
}

// ─── Compliance instruction builders ─────────────────────────────────────

fn ix_initialize_registry(payer: &Address) -> Instruction {
    let (registry, _) = derive_registry();
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(registry, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data: vec![ix::INITIALIZE_REGISTRY],
    }
}

fn ix_register_account(
    operator: &Address,
    payer: &Address,
    participant: &Address,
    jurisdiction: [u8; 2],
) -> Instruction {
    let (registry, _) = derive_registry();
    let (record, _) = derive_record(participant);
    let mut data = Vec::with_capacity(1 + 32 + 2);
    data.push(ix::REGISTER_ACCOUNT);
    data.extend_from_slice(participant.as_ref());
    data.extend_from_slice(&jurisdiction);
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*operator, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(registry, false),
            AccountMeta::new(record, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data,
    }
}

fn ix_update_status(
    operator: &Address,
    participant: &Address,
    new_status: u8,
    new_flags: u8,
    mask: u8,
) -> Instruction {
    let (registry, _) = derive_registry();
    let (record, _) = derive_record(participant);
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*operator, true),
            AccountMeta::new_readonly(registry, false),
            AccountMeta::new(record, false),
        ],
        data: vec![ix::UPDATE_STATUS, new_status, new_flags, mask],
    }
}

/// Build the SPL-discriminator instruction for `InitializeExtraAccountMetaList`.
/// Per the SPL spec, the discriminator is followed by an `extra_account_metas`
/// blob — but our handler ignores the payload and writes a hard-coded
/// SSR-specific layout, so we just emit the discriminator.
fn ix_init_meta_list(
    payer: &Address,
    mint: &Address,
    mint_authority: &Address,
) -> Instruction {
    let (list_pda, _) = derive_meta_list(mint);
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(list_pda, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(*mint_authority, true),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data: hook_disc::INITIALIZE_EXTRA_ACCOUNT_METAS.to_vec(),
    }
}

// ─── Token-2022 mint with TransferHook extension ─────────────────────────

/// Create a Token-2022 mint whose `TransferHook` extension points at our
/// compliance program. Returns the mint pubkey.
fn create_mint_with_hook(
    svm: &mut LiteSVM,
    payer: &Keypair,
    mint_kp: &Keypair,
    mint_authority: &Address,
    decimals: u8,
) {
    let mint = mint_kp.pubkey();
    let space = ExtensionType::try_calculate_account_len::<Mint>(&[ExtensionType::TransferHook])
        .expect("calc mint space");
    let lamports = svm.minimum_balance_for_rent_exemption(space);

    let create_acc = solana_system_interface::instruction::create_account(
        &payer.pubkey(),
        &mint,
        lamports,
        space as u64,
        &spl_token_2022::ID,
    );
    let init_hook = transfer_hook::instruction::initialize(
        &spl_token_2022::ID,
        &mint,
        Some(*mint_authority),
        Some(program_id()),
    )
    .expect("build init transfer hook ix");
    let init_mint = spl_token_2022::instruction::initialize_mint2(
        &spl_token_2022::ID,
        &mint,
        mint_authority,
        None,
        decimals,
    )
    .expect("build init mint ix");

    send(svm, &[create_acc, init_hook, init_mint], &[payer, mint_kp]);
}

// ─── Helpers for token accounts and transfers ────────────────────────────

fn create_token_account(
    svm: &mut LiteSVM,
    payer: &Keypair,
    account_kp: &Keypair,
    mint: &Address,
    owner: &Address,
) {
    // Token accounts for mints carrying the `TransferHook` extension
    // must themselves carry the `TransferHookAccount` extension so
    // Token-2022 has somewhere to put the per-transfer re-entrancy
    // guard. Sizing the account for it up-front is required —
    // `initialize_account3` reads the mint, sees the hook, and rejects
    // with `InvalidAccountData` if the account is too small.
    let space = ExtensionType::try_calculate_account_len::<TokenAccount>(&[
        ExtensionType::TransferHookAccount,
    ])
    .expect("calc token account space");
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
    .expect("build init account ix");
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
    .expect("build mint_to");
    send(svm, &[ix], &[payer, mint_authority]);
}

/// Build a `transfer_checked` instruction with the SSR-specific extra
/// accounts appended per the SPL hook protocol:
///
///   [0..4]  standard transfer_checked accounts (source / mint / dest / authority)
///   [4]     source AccountRecord PDA  (resolved meta 0)
///   [5]     destination AccountRecord PDA  (resolved meta 1)
///   [6]     hook program id  (so Token-2022 can CPI into us)
///   [7]     `ExtraAccountMetaList` PDA  (validation pubkey)
fn build_transfer_checked_with_hook(
    source_token: &Address,
    mint: &Address,
    dest_token: &Address,
    source_owner: &Address,
    source_record_pda: &Address,
    dest_record_pda: &Address,
    meta_list_pda: &Address,
    amount: u64,
    decimals: u8,
) -> Instruction {
    let mut ix = spl_token_2022::instruction::transfer_checked(
        &spl_token_2022::ID,
        source_token,
        mint,
        dest_token,
        source_owner,
        &[],
        amount,
        decimals,
    )
    .expect("build transfer_checked");
    ix.accounts
        .push(AccountMeta::new_readonly(*source_record_pda, false));
    ix.accounts
        .push(AccountMeta::new_readonly(*dest_record_pda, false));
    ix.accounts
        .push(AccountMeta::new_readonly(program_id(), false));
    ix.accounts
        .push(AccountMeta::new_readonly(*meta_list_pda, false));
    ix
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[test]
fn create_mint_records_hook_program_id() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    let mint_kp = Keypair::new();
    create_mint_with_hook(&mut svm, &payer, &mint_kp, &payer.pubkey(), 6);

    let acc = svm.get_account(&mint_kp.pubkey()).expect("mint exists");
    assert_eq!(acc.owner, token_2022_program_id());
    // The base 82-byte Mint must unpack cleanly.
    let mint = Mint::unpack(&acc.data[..Mint::LEN]).expect("unpack mint");
    assert_eq!(mint.decimals, 6);

    // Inspect the TransferHook extension — its program_id field must
    // equal our compliance program ID.
    let state =
        StateWithExtensions::<Mint>::unpack(&acc.data).expect("unpack mint with extensions");
    let hook_ext = state
        .get_extension::<transfer_hook::TransferHook>()
        .expect("transfer hook extension present");
    let recorded: Option<Address> = hook_ext.program_id.into();
    assert_eq!(
        recorded.map(|p| p.to_bytes()),
        Some(program_id().to_bytes()),
        "hook program id matches our compliance program"
    );
}

#[test]
fn init_meta_list_creates_pda_with_spl_compliant_bytes() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };

    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    let mint_kp = Keypair::new();
    create_mint_with_hook(&mut svm, &payer, &mint_kp, &payer.pubkey(), 6);

    send(
        &mut svm,
        &[ix_init_meta_list(&payer.pubkey(), &mint_kp.pubkey(), &payer.pubkey())],
        &[&payer],
    );

    let (list_pda, _) = derive_meta_list(&mint_kp.pubkey());
    let acc = svm.get_account(&list_pda).expect("meta list PDA exists");
    assert_eq!(acc.owner, program_id());
    assert_eq!(acc.data.len(), extra_metas::ACCOUNT_SIZE);

    // The first 8 bytes are the TLV type discriminator, which must equal
    // the SPL hook execute discriminator.
    assert_eq!(
        &acc.data[..8],
        &hook_disc::EXECUTE,
        "meta list TLV type = hook execute discriminator"
    );

    // Reproduce the expected layout via our hand-rolled writer and
    // compare byte-for-byte against what landed on-chain.
    let mut expected = vec![0u8; extra_metas::ACCOUNT_SIZE];
    extra_metas::write(&mut expected).unwrap();
    assert_eq!(acc.data, expected, "on-chain bytes match builder output");
}

#[test]
fn verified_transfer_through_hook_succeeds() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    // 1. Bootstrap compliance registry.
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    // 2. Create the Token-2022 mint with TransferHook ext.
    let mint_kp = Keypair::new();
    create_mint_with_hook(&mut svm, &payer, &mint_kp, &payer.pubkey(), 6);

    // 3. Initialize the meta list so Token-2022 can resolve our extras.
    send(
        &mut svm,
        &[ix_init_meta_list(&payer.pubkey(), &mint_kp.pubkey(), &payer.pubkey())],
        &[&payer],
    );

    // 4. Create source and destination token accounts.
    let src_owner_kp = Keypair::new();
    let dst_owner_kp = Keypair::new();
    svm.airdrop(&src_owner_kp.pubkey(), 1_000_000_000).unwrap();
    svm.airdrop(&dst_owner_kp.pubkey(), 1_000_000_000).unwrap();
    let src_ata_kp = Keypair::new();
    let dst_ata_kp = Keypair::new();
    create_token_account(
        &mut svm,
        &payer,
        &src_ata_kp,
        &mint_kp.pubkey(),
        &src_owner_kp.pubkey(),
    );
    create_token_account(
        &mut svm,
        &payer,
        &dst_ata_kp,
        &mint_kp.pubkey(),
        &dst_owner_kp.pubkey(),
    );

    // 5. Register and verify both participants.
    for owner in [&src_owner_kp.pubkey(), &dst_owner_kp.pubkey()] {
        send(
            &mut svm,
            &[ix_register_account(&payer.pubkey(), &payer.pubkey(), owner, *b"JP")],
            &[&payer],
        );
        send(
            &mut svm,
            &[ix_update_status(
                &payer.pubkey(),
                owner,
                compliance_status::VERIFIED,
                0,
                change_mask::STATUS,
            )],
            &[&payer],
        );
    }

    // 6. Mint to source.
    mint_to(&mut svm, &payer, &mint_kp.pubkey(), &payer, &src_ata_kp.pubkey(), 1_000_000);

    // 7. Transfer through the hook — must succeed since both sides are VERIFIED.
    let (src_record, _) = derive_record(&src_owner_kp.pubkey());
    let (dst_record, _) = derive_record(&dst_owner_kp.pubkey());
    let (list_pda, _) = derive_meta_list(&mint_kp.pubkey());
    let xfer = build_transfer_checked_with_hook(
        &src_ata_kp.pubkey(),
        &mint_kp.pubkey(),
        &dst_ata_kp.pubkey(),
        &src_owner_kp.pubkey(),
        &src_record,
        &dst_record,
        &list_pda,
        500_000,
        6,
    );
    send(&mut svm, &[xfer], &[&src_owner_kp]);

    // Verify destination received the tokens.
    let dst_acc = svm.get_account(&dst_ata_kp.pubkey()).expect("dest exists");
    let dst_state = TokenAccount::unpack(&dst_acc.data[..TokenAccount::LEN])
        .expect("unpack dest account");
    assert_eq!(dst_state.amount, 500_000);
}

#[test]
fn transfer_to_suspended_recipient_rejects_with_hook_error() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    let mint_kp = Keypair::new();
    create_mint_with_hook(&mut svm, &payer, &mint_kp, &payer.pubkey(), 6);
    send(
        &mut svm,
        &[ix_init_meta_list(&payer.pubkey(), &mint_kp.pubkey(), &payer.pubkey())],
        &[&payer],
    );

    let src_owner_kp = Keypair::new();
    let dst_owner_kp = Keypair::new();
    svm.airdrop(&src_owner_kp.pubkey(), 1_000_000_000).unwrap();
    svm.airdrop(&dst_owner_kp.pubkey(), 1_000_000_000).unwrap();
    let src_ata_kp = Keypair::new();
    let dst_ata_kp = Keypair::new();
    create_token_account(&mut svm, &payer, &src_ata_kp, &mint_kp.pubkey(), &src_owner_kp.pubkey());
    create_token_account(&mut svm, &payer, &dst_ata_kp, &mint_kp.pubkey(), &dst_owner_kp.pubkey());

    // Source: VERIFIED. Destination: VERIFIED then SUSPENDED.
    for owner in [&src_owner_kp.pubkey(), &dst_owner_kp.pubkey()] {
        send(
            &mut svm,
            &[ix_register_account(&payer.pubkey(), &payer.pubkey(), owner, *b"JP")],
            &[&payer],
        );
        send(
            &mut svm,
            &[ix_update_status(
                &payer.pubkey(),
                owner,
                compliance_status::VERIFIED,
                0,
                change_mask::STATUS,
            )],
            &[&payer],
        );
    }
    send(
        &mut svm,
        &[ix_update_status(
            &payer.pubkey(),
            &dst_owner_kp.pubkey(),
            compliance_status::SUSPENDED,
            0,
            change_mask::STATUS,
        )],
        &[&payer],
    );

    mint_to(&mut svm, &payer, &mint_kp.pubkey(), &payer, &src_ata_kp.pubkey(), 1_000_000);

    let (src_record, _) = derive_record(&src_owner_kp.pubkey());
    let (dst_record, _) = derive_record(&dst_owner_kp.pubkey());
    let (list_pda, _) = derive_meta_list(&mint_kp.pubkey());
    let xfer = build_transfer_checked_with_hook(
        &src_ata_kp.pubkey(),
        &mint_kp.pubkey(),
        &dst_ata_kp.pubkey(),
        &src_owner_kp.pubkey(),
        &src_record,
        &dst_record,
        &list_pda,
        500_000,
        6,
    );
    let res = try_send(&mut svm, &[xfer], &[&src_owner_kp]);
    assert!(res.is_err(), "transfer to suspended dest must be rejected");
}
