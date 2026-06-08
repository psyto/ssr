//! LiteSVM smoke tests for the SSR compliance program write-side.
//!
//! These tests exercise the actual BPF program in an in-process Solana VM.
//! They confirm that:
//!   1. `initialize_registry` allocates the registry PDA and writes the
//!      expected `Registry` byte layout, with the payer recorded as
//!      `super_admin`.
//!   2. `register_account` allocates an `AccountRecord` PDA for a fresh
//!      participant, in `PENDING` status, signed by `onboard_operator`.
//!   3. `update_status` transitions `PENDING → VERIFIED` when signed by
//!      `status_operator` and the transition satisfies policy.
//!
//! Prerequisite: the BPF artifact at `target/deploy/ssr_compliance.so`
//! must exist. Build it with `cargo build-sbf --manifest-path
//! programs/ssr-compliance/Cargo.toml`. If the artifact is missing the
//! tests skip with an informative message rather than failing — this
//! keeps `cargo test --workspace` green on hosts without the Solana
//! SBF toolchain.

use {
    bytemuck::from_bytes,
    litesvm::LiteSVM,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_message::Message,
    solana_address::Address,
    solana_signer::Signer,
    solana_transaction::Transaction,
    ssr_compliance::{change_mask, err, ix},
    ssr_types::{
        asset_class, compliance_status, role, seeds, AccountRecord, PriceFeed, PythConfig,
        Registry, RiskParams, DEFAULT_HAIRCUTS,
    },
    std::path::PathBuf,
};

// ─── Test harness ────────────────────────────────────────────────────────

/// Fixed program ID for tests. The actual deployed program ID can be
/// different in production — this constant is only what LiteSVM thinks
/// the program lives at. Built from a hand-picked byte pattern so
/// PDA derivation is deterministic across test runs without depending
/// on base58 parsing.
fn program_id() -> Address {
    Address::from([7u8; 32])
}

fn so_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../target/deploy/ssr_compliance.so");
    p
}

/// Set up a fresh `LiteSVM` instance with the compliance program loaded
/// and a funded payer. Returns `None` if the BPF artifact is missing,
/// signaling that the calling test should skip.
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
    svm.airdrop(&payer.pubkey(), 10_000_000_000).expect("airdrop");
    Some((svm, payer))
}

fn send(svm: &mut LiteSVM, ixs: &[Instruction], signers: &[&Keypair]) {
    let blockhash = svm.latest_blockhash();
    let payer = signers[0].pubkey();
    let msg = Message::new(ixs, Some(&payer));
    let tx = Transaction::new(signers, msg, blockhash);
    let res = svm.send_transaction(tx);
    if let Err(e) = res {
        panic!("tx failed: {e:?}");
    }
}

/// Variant for negative tests: returns the err string when the tx
/// fails. We stringify because `litesvm`'s `FailedTransactionMetadata`
/// doesn't expose the inner `ProgramError` shape directly — Debug
/// output includes `"Custom(0x10XX)"` which we substring-match.
fn try_send(
    svm: &mut LiteSVM,
    ixs: &[Instruction],
    signers: &[&Keypair],
) -> Result<(), String> {
    let blockhash = svm.latest_blockhash();
    let payer = signers[0].pubkey();
    let msg = Message::new(ixs, Some(&payer));
    let tx = Transaction::new(signers, msg, blockhash);
    svm.send_transaction(tx).map(|_| ()).map_err(|e| format!("{e:?}"))
}

fn derive_registry() -> (Address, u8) {
    Address::find_program_address(&[seeds::REGISTRY], &program_id())
}

fn derive_account_record(participant: &Address) -> (Address, u8) {
    Address::find_program_address(
        &[seeds::ACCOUNT_RECORD, participant.as_ref()],
        &program_id(),
    )
}

fn derive_risk_params() -> (Address, u8) {
    Address::find_program_address(&[seeds::RISK_PARAMS], &program_id())
}

// ─── Instruction builders ────────────────────────────────────────────────

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
    let (record, _) = derive_account_record(participant);
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

fn ix_initialize_risk_params(admin: &Address, payer: &Address) -> Instruction {
    let (registry, _) = derive_registry();
    let (risk, _) = derive_risk_params();
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(registry, false),
            AccountMeta::new(risk, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data: vec![ix::INITIALIZE_RISK_PARAMS],
    }
}

fn ix_set_haircut(admin: &Address, class: u8, bps: u16) -> Instruction {
    let (registry, _) = derive_registry();
    let (risk, _) = derive_risk_params();
    let mut data = Vec::with_capacity(1 + 1 + 2);
    data.push(ix::SET_HAIRCUT);
    data.push(class);
    data.extend_from_slice(&bps.to_le_bytes());
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new_readonly(registry, false),
            AccountMeta::new(risk, false),
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
    let (record, _) = derive_account_record(participant);
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

// ─── Tests ───────────────────────────────────────────────────────────────

#[test]
fn initialize_registry_creates_pda_with_payer_as_super_admin() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };

    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    let (registry_pda, expected_bump) = derive_registry();
    let acc = svm
        .get_account(&registry_pda)
        .expect("registry account exists");
    assert_eq!(acc.owner, program_id(), "registry owner is program");
    assert_eq!(
        acc.data.len(),
        Registry::LEN,
        "registry sized at Registry::LEN"
    );

    let r: &Registry = from_bytes(&acc.data[..Registry::LEN]);
    assert_eq!(r.super_admin, payer.pubkey().to_bytes());
    // Initial registry: operational roles default to super_admin.
    assert_eq!(r.onboard_operator, payer.pubkey().to_bytes());
    assert_eq!(r.status_operator, payer.pubkey().to_bytes());
    assert_eq!(r.version, Registry::CURRENT_VERSION);
    assert_eq!(r.bump, expected_bump);
}

#[test]
fn register_account_creates_pending_record() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };

    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    let participant = Address::new_unique();
    let jurisdiction = *b"JP";
    send(
        &mut svm,
        &[ix_register_account(
            &payer.pubkey(),
            &payer.pubkey(),
            &participant,
            jurisdiction,
        )],
        &[&payer],
    );

    let (record_pda, expected_bump) = derive_account_record(&participant);
    let acc = svm
        .get_account(&record_pda)
        .expect("account_record exists");
    assert_eq!(acc.owner, program_id());
    assert_eq!(acc.data.len(), AccountRecord::LEN);

    let rec: &AccountRecord = from_bytes(&acc.data[..AccountRecord::LEN]);
    assert_eq!(rec.participant, participant.to_bytes());
    assert_eq!(rec.status, compliance_status::PENDING);
    assert_eq!(rec.jurisdiction, jurisdiction);
    assert_eq!(rec.flags, 0);
    assert_eq!(rec.bump, expected_bump);
}

#[test]
fn update_status_promotes_pending_to_verified() {
    let Some((mut svm, payer)) = setup() else {
        return;
    };

    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    let participant = Address::new_unique();
    send(
        &mut svm,
        &[ix_register_account(
            &payer.pubkey(),
            &payer.pubkey(),
            &participant,
            *b"JP",
        )],
        &[&payer],
    );

    send(
        &mut svm,
        &[ix_update_status(
            &payer.pubkey(),
            &participant,
            compliance_status::VERIFIED,
            0,
            change_mask::STATUS,
        )],
        &[&payer],
    );

    let (record_pda, _) = derive_account_record(&participant);
    let acc = svm.get_account(&record_pda).expect("account_record exists");
    let rec: &AccountRecord = from_bytes(&acc.data[..AccountRecord::LEN]);
    assert_eq!(rec.status, compliance_status::VERIFIED);
    assert!(rec.is_verified());
}

// ─── Phase 4 v1c: RiskParams tests ──────────────────────────────────────-

#[test]
fn initialize_risk_params_creates_pda_with_default_haircuts() {
    let Some((mut svm, payer)) = setup() else { return };

    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    send(
        &mut svm,
        &[ix_initialize_risk_params(&payer.pubkey(), &payer.pubkey())],
        &[&payer],
    );

    let (risk_pda, expected_bump) = derive_risk_params();
    let acc = svm.get_account(&risk_pda).expect("risk_params PDA exists");
    assert_eq!(acc.owner, program_id());
    assert_eq!(acc.data.len(), RiskParams::LEN);

    let rp: &RiskParams = from_bytes(&acc.data[..RiskParams::LEN]);
    assert_eq!(rp.version, RiskParams::CURRENT_VERSION);
    assert_eq!(rp.bump, expected_bump);
    assert_eq!(rp.haircut_bps, DEFAULT_HAIRCUTS);
    // Spot-check: the on-chain default table must agree with the
    // CLI-side fallback the docs promise. If these drift, pre- and
    // post-init demos disagree on the same vault's haircut.
    assert_eq!(rp.haircut_for(asset_class::STABLECOIN), 0);
    assert_eq!(rp.haircut_for(asset_class::EQUITY), 3_000);
    assert_eq!(rp.haircut_for(asset_class::UNKNOWN), 10_000);
}

#[test]
fn initialize_risk_params_rejects_non_super_admin() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    let imposter = Keypair::new();
    svm.airdrop(&imposter.pubkey(), 1_000_000_000).expect("airdrop");
    let err_str = try_send(
        &mut svm,
        &[ix_initialize_risk_params(&imposter.pubkey(), &imposter.pubkey())],
        &[&imposter],
    )
    .expect_err("init must reject when caller isn't super_admin");
    assert!(
        err_str.contains("Custom(4129)"),
        "expected UNAUTHORIZED_SUPER_ADMIN (0x1021 = 4129), got: {err_str}"
    );
}

#[test]
fn set_haircut_updates_one_cell_and_leaves_others_unchanged() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    send(
        &mut svm,
        &[ix_initialize_risk_params(&payer.pubkey(), &payer.pubkey())],
        &[&payer],
    );

    // Tighten equity from default 3_000 to 4_500.
    let new_bps = 4_500u16;
    send(
        &mut svm,
        &[ix_set_haircut(&payer.pubkey(), asset_class::EQUITY, new_bps)],
        &[&payer],
    );

    let (risk_pda, _) = derive_risk_params();
    let acc = svm.get_account(&risk_pda).unwrap();
    let rp: &RiskParams = from_bytes(&acc.data[..RiskParams::LEN]);
    assert_eq!(rp.haircut_for(asset_class::EQUITY), new_bps);
    // Every other entry should still match the seeded defaults.
    for c in 0u8..(RiskParams::HAIRCUT_TABLE_LEN as u8) {
        if c == asset_class::EQUITY {
            continue;
        }
        assert_eq!(
            rp.haircut_for(c),
            DEFAULT_HAIRCUTS[c as usize],
            "class {c} should remain at its default"
        );
    }
}

#[test]
fn set_haircut_rejects_non_super_admin() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    send(
        &mut svm,
        &[ix_initialize_risk_params(&payer.pubkey(), &payer.pubkey())],
        &[&payer],
    );

    let imposter = Keypair::new();
    svm.airdrop(&imposter.pubkey(), 1_000_000_000).expect("airdrop");
    let err_str = try_send(
        &mut svm,
        &[ix_set_haircut(&imposter.pubkey(), asset_class::EQUITY, 4_500)],
        &[&imposter],
    )
    .expect_err("set_haircut must reject non-super-admin");
    assert!(
        err_str.contains("Custom(4129)"),
        "expected UNAUTHORIZED_SUPER_ADMIN (0x1021 = 4129), got: {err_str}"
    );
}

#[test]
fn set_haircut_rejects_bps_over_10000() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    send(
        &mut svm,
        &[ix_initialize_risk_params(&payer.pubkey(), &payer.pubkey())],
        &[&payer],
    );

    let err_str = try_send(
        &mut svm,
        &[ix_set_haircut(&payer.pubkey(), asset_class::EQUITY, 10_001)],
        &[&payer],
    )
    .expect_err("set_haircut must reject bps > 10_000");
    // HAIRCUT_OUT_OF_RANGE = 0x1033 = 4147
    assert!(
        err_str.contains("Custom(4147)"),
        "expected HAIRCUT_OUT_OF_RANGE (0x1033 = 4147), got: {err_str}"
    );
}

#[test]
fn set_haircut_rejects_class_above_table() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    send(
        &mut svm,
        &[ix_initialize_risk_params(&payer.pubkey(), &payer.pubkey())],
        &[&payer],
    );

    // HAIRCUT_TABLE_LEN = 32, so 32 is the first invalid index.
    let err_str = try_send(
        &mut svm,
        &[ix_set_haircut(&payer.pubkey(), 32, 1_000)],
        &[&payer],
    )
    .expect_err("set_haircut must reject class >= HAIRCUT_TABLE_LEN");
    // ASSET_CLASS_OUT_OF_RANGE = 0x1032 = 4146
    assert!(
        err_str.contains("Custom(4146)"),
        "expected ASSET_CLASS_OUT_OF_RANGE (0x1032 = 4146), got: {err_str}"
    );
}

// Quiet a warning when the err namespace is imported but the
// negative tests above skip on hosts without the SBF artifact.
#[cfg(test)]
#[allow(dead_code)]
fn _unused_err_link() {
    let _ = err::UNAUTHORIZED_SUPER_ADMIN;
    let _ = err::HAIRCUT_OUT_OF_RANGE;
    let _ = err::ASSET_CLASS_OUT_OF_RANGE;
    let _ = err::UNAUTHORIZED_ORACLE;
    let _ = err::PRICE_FEED_PDA_MISMATCH;
}

// ─── Phase 4 v1d: PriceFeed + RiskParams.max_staleness tests ────────────-

fn derive_price_feed(mint: &Address) -> (Address, u8) {
    Address::find_program_address(&[seeds::PRICE_FEED, mint.as_ref()], &program_id())
}

fn ix_register_price_feed(
    admin: &Address,
    payer: &Address,
    mint: &Address,
    price_micro_usd: u64,
    mint_decimals: u8,
) -> Instruction {
    let (feed, _) = derive_price_feed(mint);
    let mut data = Vec::with_capacity(1 + 32 + 8 + 1);
    data.push(ix::REGISTER_PRICE_FEED);
    data.extend_from_slice(mint.as_ref());
    data.extend_from_slice(&price_micro_usd.to_le_bytes());
    data.push(mint_decimals);
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(derive_registry().0, false),
            AccountMeta::new(feed, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data,
    }
}

fn ix_update_price(oracle: &Address, mint: &Address, new_price: u64) -> Instruction {
    let (feed, _) = derive_price_feed(mint);
    let mut data = Vec::with_capacity(1 + 8);
    data.push(ix::UPDATE_PRICE);
    data.extend_from_slice(&new_price.to_le_bytes());
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*oracle, true),
            AccountMeta::new_readonly(derive_registry().0, false),
            AccountMeta::new(feed, false),
        ],
        data,
    }
}

fn ix_set_max_staleness(admin: &Address, new_max: u64) -> Instruction {
    let (risk, _) = derive_risk_params();
    let mut data = Vec::with_capacity(1 + 8);
    data.push(ix::SET_MAX_STALENESS);
    data.extend_from_slice(&new_max.to_le_bytes());
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new_readonly(derive_registry().0, false),
            AccountMeta::new(risk, false),
        ],
        data,
    }
}

fn ix_rotate_operator(admin: &Address, target_role: u8, new_pubkey: &Address) -> Instruction {
    let mut data = Vec::with_capacity(1 + 1 + 32);
    data.push(ix::ROTATE_OPERATORS);
    data.push(target_role);
    data.extend_from_slice(new_pubkey.as_ref());
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(derive_registry().0, false),
        ],
        data,
    }
}

#[test]
fn register_price_feed_creates_pda_with_initial_price_and_decimals() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    let mint = Address::new_unique();
    send(
        &mut svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &mint, 1_050_000, 6)],
        &[&payer],
    );

    let (feed_pda, expected_bump) = derive_price_feed(&mint);
    let acc = svm.get_account(&feed_pda).expect("price_feed PDA exists");
    assert_eq!(acc.owner, program_id());
    assert_eq!(acc.data.len(), PriceFeed::LEN);
    let pf: &PriceFeed = from_bytes(&acc.data[..PriceFeed::LEN]);
    assert_eq!(pf.mint, mint.to_bytes());
    assert_eq!(pf.price_micro_usd, 1_050_000);
    assert_eq!(pf.mint_decimals, 6);
    assert_eq!(pf.version, PriceFeed::CURRENT_VERSION);
    assert_eq!(pf.bump, expected_bump);
}

#[test]
fn register_price_feed_rejects_non_super_admin() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    let imposter = Keypair::new();
    svm.airdrop(&imposter.pubkey(), 1_000_000_000).unwrap();
    let mint = Address::new_unique();
    let err = try_send(
        &mut svm,
        &[ix_register_price_feed(&imposter.pubkey(), &imposter.pubkey(), &mint, 1, 6)],
        &[&imposter],
    )
    .expect_err("register_price_feed must reject non-super-admin");
    // UNAUTHORIZED_SUPER_ADMIN = 0x1021 = 4129
    assert!(err.contains("Custom(4129)"), "expected 0x1021, got: {err}");
}

#[test]
fn update_price_by_oracle_operator_succeeds() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    // Rotate the oracle role to a dedicated key (registry init
    // defaults all roles to super_admin).
    let oracle = Keypair::new();
    svm.airdrop(&oracle.pubkey(), 1_000_000_000).unwrap();
    send(
        &mut svm,
        &[ix_rotate_operator(&payer.pubkey(), role::ORACLE, &oracle.pubkey())],
        &[&payer],
    );

    let mint = Address::new_unique();
    send(
        &mut svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &mint, 1_000_000, 6)],
        &[&payer],
    );

    // Update — should succeed under the rotated oracle role.
    send(&mut svm, &[ix_update_price(&oracle.pubkey(), &mint, 2_500_000)], &[&oracle]);
    let (feed_pda, _) = derive_price_feed(&mint);
    let acc = svm.get_account(&feed_pda).unwrap();
    let pf: &PriceFeed = from_bytes(&acc.data[..PriceFeed::LEN]);
    assert_eq!(pf.price_micro_usd, 2_500_000, "price reflects update");
    // last_updated_slot is set to Clock::get()?.slot. LiteSVM stays
    // at slot 0 unless explicitly warped, so we don't try to assert
    // a non-zero value — the price change above proves the handler
    // ran to completion and the field was assigned.
}

#[test]
fn update_price_by_non_oracle_rejects() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    // Rotate oracle to a key that ISN'T the payer.
    let oracle = Keypair::new();
    svm.airdrop(&oracle.pubkey(), 1_000_000_000).unwrap();
    send(
        &mut svm,
        &[ix_rotate_operator(&payer.pubkey(), role::ORACLE, &oracle.pubkey())],
        &[&payer],
    );

    let mint = Address::new_unique();
    send(
        &mut svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &mint, 1_000_000, 6)],
        &[&payer],
    );

    // Imposter tries to update.
    let imposter = Keypair::new();
    svm.airdrop(&imposter.pubkey(), 1_000_000_000).unwrap();
    let err = try_send(
        &mut svm,
        &[ix_update_price(&imposter.pubkey(), &mint, 999)],
        &[&imposter],
    )
    .expect_err("update_price must reject non-oracle");
    // UNAUTHORIZED_ORACLE = 0x1042 = 4162
    assert!(err.contains("Custom(4162)"), "expected 0x1042, got: {err}");
}

#[test]
fn set_max_staleness_by_super_admin_updates_risk_params() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    send(
        &mut svm,
        &[ix_initialize_risk_params(&payer.pubkey(), &payer.pubkey())],
        &[&payer],
    );

    let initial_max = {
        let (rp_pda, _) = derive_risk_params();
        let acc = svm.get_account(&rp_pda).unwrap();
        let rp: &RiskParams = from_bytes(&acc.data[..RiskParams::LEN]);
        rp.max_staleness_slots
    };
    assert_eq!(initial_max, RiskParams::DEFAULT_MAX_STALENESS_SLOTS);

    send(&mut svm, &[ix_set_max_staleness(&payer.pubkey(), 3_000)], &[&payer]);
    let (rp_pda, _) = derive_risk_params();
    let acc = svm.get_account(&rp_pda).unwrap();
    let rp: &RiskParams = from_bytes(&acc.data[..RiskParams::LEN]);
    assert_eq!(rp.max_staleness_slots, 3_000);
}

#[test]
fn set_max_staleness_by_non_super_rejects() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    send(
        &mut svm,
        &[ix_initialize_risk_params(&payer.pubkey(), &payer.pubkey())],
        &[&payer],
    );

    let imposter = Keypair::new();
    svm.airdrop(&imposter.pubkey(), 1_000_000_000).unwrap();
    let err = try_send(
        &mut svm,
        &[ix_set_max_staleness(&imposter.pubkey(), 100)],
        &[&imposter],
    )
    .expect_err("set_max_staleness must reject non-super");
    assert!(err.contains("Custom(4129)"), "expected 0x1021, got: {err}");
}

#[test]
fn rotate_oracle_role_sets_oracle_operator_field() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    let new_oracle = Address::new_unique();
    send(
        &mut svm,
        &[ix_rotate_operator(&payer.pubkey(), role::ORACLE, &new_oracle)],
        &[&payer],
    );
    let (reg_pda, _) = derive_registry();
    let acc = svm.get_account(&reg_pda).unwrap();
    let r: &Registry = from_bytes(&acc.data[..Registry::LEN]);
    assert_eq!(r.oracle_operator, new_oracle.to_bytes());
    // Other roles unchanged.
    assert_eq!(r.onboard_operator, payer.pubkey().to_bytes());
    assert_eq!(r.status_operator, payer.pubkey().to_bytes());
}

// ─── Phase 4 v1f: Pyth-adapter tests ────────────────────────────────────-

// Anchor discriminator: sha256("account:PriceUpdateV2")[..8]. Pinned
// here so the test surface fails loudly if Pyth ever migrates the
// account name and our hard-coded constant in
// `ssr-compliance::PYTH_PRICE_UPDATE_V2_DISCRIMINATOR` falls out of
// sync.
const PYTH_DISCRIMINATOR: [u8; 8] = [34, 241, 35, 99, 157, 126, 244, 205];

#[test]
fn pyth_discriminator_matches_sha256() {
    use sha2::{Digest, Sha256};
    let computed = Sha256::digest(b"account:PriceUpdateV2");
    assert_eq!(&computed[..8], &PYTH_DISCRIMINATOR);
}

/// Build a 134-byte `PriceUpdateV2`-layout buffer with the given
/// price/conf/exponent. write_authority / feed_id / timestamps /
/// ema fields are left zero — the SSR handler only reads price +
/// conf + exponent.
fn mock_pyth_account_data(price: i64, conf: u64, exponent: i32) -> Vec<u8> {
    let mut buf = vec![0u8; 134];
    buf[..8].copy_from_slice(&PYTH_DISCRIMINATOR);
    // write_authority [8..40]: leave zero
    // verification_level [40..42]: tag=1 (Full), payload=0
    buf[40] = 1;
    // feed_id [42..74]: leave zero
    buf[74..82].copy_from_slice(&price.to_le_bytes());
    buf[82..90].copy_from_slice(&conf.to_le_bytes());
    buf[90..94].copy_from_slice(&exponent.to_le_bytes());
    // remaining fields zero
    buf
}

/// Inject a mock Pyth account into the test SVM at the given
/// address. Owner is set to a placeholder (we don't validate
/// Pyth's program ID; the trust point is the address binding).
fn set_mock_pyth_account(svm: &mut LiteSVM, addr: Address, price: i64, conf: u64, exponent: i32) {
    let data = mock_pyth_account_data(price, conf, exponent);
    let acc = solana_account::Account {
        lamports: 10_000_000,
        data,
        owner: Address::new_unique(), // any non-system owner; the SSR handler doesn't check it
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(addr, acc).expect("set mock pyth account");
}

fn ix_bind_price_feed_to_pyth(
    admin: &Address,
    mint: &Address,
    pyth_source: &Address,
) -> Instruction {
    let (feed, _) = derive_price_feed(mint);
    let mut data = Vec::with_capacity(1 + 32);
    data.push(ix::BIND_PRICE_FEED_TO_PYTH);
    data.extend_from_slice(pyth_source.as_ref());
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new_readonly(derive_registry().0, false),
            AccountMeta::new(feed, false),
        ],
        data,
    }
}

fn ix_update_price_from_pyth(
    oracle: &Address,
    mint: &Address,
    pyth_source: &Address,
) -> Instruction {
    let (feed, _) = derive_price_feed(mint);
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*oracle, true),
            AccountMeta::new_readonly(derive_registry().0, false),
            AccountMeta::new(feed, false),
            AccountMeta::new_readonly(*pyth_source, false),
        ],
        data: vec![ix::UPDATE_PRICE_FROM_PYTH],
    }
}

#[test]
fn bind_price_feed_to_pyth_sets_source() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    let mint = Address::new_unique();
    send(
        &mut svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &mint, 1_000_000, 6)],
        &[&payer],
    );

    let pyth_addr = Address::new_unique();
    send(
        &mut svm,
        &[ix_bind_price_feed_to_pyth(&payer.pubkey(), &mint, &pyth_addr)],
        &[&payer],
    );

    let (feed_pda, _) = derive_price_feed(&mint);
    let acc = svm.get_account(&feed_pda).unwrap();
    let pf: &PriceFeed = from_bytes(&acc.data[..PriceFeed::LEN]);
    assert_eq!(pf.pyth_source, pyth_addr.to_bytes());
    assert!(pf.is_pyth_bound());
}

#[test]
fn update_price_from_pyth_applies_conservative_price_and_normalizes_exponent() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    let mint = Address::new_unique();
    send(
        &mut svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &mint, 1_000_000, 6)],
        &[&payer],
    );

    let pyth_addr = Address::new_unique();
    send(
        &mut svm,
        &[ix_bind_price_feed_to_pyth(&payer.pubkey(), &mint, &pyth_addr)],
        &[&payer],
    );
    // Pyth-format account: price 105_000_000_000, conf 50_000_000,
    // exponent -8. Real-world meaning: $1050.00 ± $0.50. After
    // confidence subtract: 104_950_000_000. exp + 6 = -2 → divide
    // by 100 → 1_049_500_000 micro-USD = $1049.50.
    set_mock_pyth_account(&mut svm, pyth_addr, 105_000_000_000, 50_000_000, -8);

    send(
        &mut svm,
        &[ix_update_price_from_pyth(&payer.pubkey(), &mint, &pyth_addr)],
        &[&payer],
    );

    let (feed_pda, _) = derive_price_feed(&mint);
    let acc = svm.get_account(&feed_pda).unwrap();
    let pf: &PriceFeed = from_bytes(&acc.data[..PriceFeed::LEN]);
    assert_eq!(
        pf.price_micro_usd, 1_049_500_000,
        "conservative price * 10^(exp+6): (105_000_000_000 - 50_000_000) / 100"
    );
}

#[test]
fn update_price_from_pyth_rejects_unbound_feed() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    let mint = Address::new_unique();
    send(
        &mut svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &mint, 1_000_000, 6)],
        &[&payer],
    );

    // Skip the bind step; feed's pyth_source is still [0; 32].
    let pyth_addr = Address::new_unique();
    set_mock_pyth_account(&mut svm, pyth_addr, 100, 1, -6);

    let err = try_send(
        &mut svm,
        &[ix_update_price_from_pyth(&payer.pubkey(), &mint, &pyth_addr)],
        &[&payer],
    )
    .expect_err("unbound feed must reject");
    // PRICE_FEED_NOT_PYTH_BOUND = 0x1043 = 4163
    assert!(err.contains("Custom(4163)"), "expected 0x1043, got: {err}");
}

#[test]
fn update_price_from_pyth_rejects_substituted_source() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    let mint = Address::new_unique();
    send(
        &mut svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &mint, 1_000_000, 6)],
        &[&payer],
    );

    let bound = Address::new_unique();
    send(
        &mut svm,
        &[ix_bind_price_feed_to_pyth(&payer.pubkey(), &mint, &bound)],
        &[&payer],
    );
    // Caller passes a DIFFERENT Pyth account than the one bound.
    let imposter = Address::new_unique();
    set_mock_pyth_account(&mut svm, imposter, 999_999_999_999, 0, -6);

    let err = try_send(
        &mut svm,
        &[ix_update_price_from_pyth(&payer.pubkey(), &mint, &imposter)],
        &[&payer],
    )
    .expect_err("substituted pyth source must reject");
    // PYTH_SOURCE_MISMATCH = 0x1044 = 4164
    assert!(err.contains("Custom(4164)"), "expected 0x1044, got: {err}");
}

#[test]
fn update_price_from_pyth_rejects_wrong_discriminator() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    let mint = Address::new_unique();
    send(
        &mut svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &mint, 1_000_000, 6)],
        &[&payer],
    );

    let pyth_addr = Address::new_unique();
    send(
        &mut svm,
        &[ix_bind_price_feed_to_pyth(&payer.pubkey(), &mint, &pyth_addr)],
        &[&payer],
    );
    // Bound account exists but its data has the wrong discriminator.
    let mut bad = mock_pyth_account_data(100_000_000, 0, -6);
    bad[0] = 0xff;
    let acc = solana_account::Account {
        lamports: 10_000_000,
        data: bad,
        owner: Address::new_unique(),
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(pyth_addr, acc).unwrap();

    let err = try_send(
        &mut svm,
        &[ix_update_price_from_pyth(&payer.pubkey(), &mint, &pyth_addr)],
        &[&payer],
    )
    .expect_err("wrong discriminator must reject");
    // PYTH_ACCOUNT_INVALID = 0x1045 = 4165
    assert!(err.contains("Custom(4165)"), "expected 0x1045, got: {err}");
}

#[test]
fn update_price_from_pyth_rejects_negative_price() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    let mint = Address::new_unique();
    send(
        &mut svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &mint, 1_000_000, 6)],
        &[&payer],
    );

    let pyth_addr = Address::new_unique();
    send(
        &mut svm,
        &[ix_bind_price_feed_to_pyth(&payer.pubkey(), &mint, &pyth_addr)],
        &[&payer],
    );
    // Raw price -1: handler treats as broken feed.
    set_mock_pyth_account(&mut svm, pyth_addr, -1, 0, -6);

    let err = try_send(
        &mut svm,
        &[ix_update_price_from_pyth(&payer.pubkey(), &mint, &pyth_addr)],
        &[&payer],
    )
    .expect_err("negative price must reject");
    // PYTH_NEGATIVE_PRICE = 0x1046 = 4166
    assert!(err.contains("Custom(4166)"), "expected 0x1046, got: {err}");
}

#[test]
fn update_price_from_pyth_rejects_conf_greater_than_price() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    let mint = Address::new_unique();
    send(
        &mut svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &mint, 1_000_000, 6)],
        &[&payer],
    );

    let pyth_addr = Address::new_unique();
    send(
        &mut svm,
        &[ix_bind_price_feed_to_pyth(&payer.pubkey(), &mint, &pyth_addr)],
        &[&payer],
    );
    // Price 100, conf 200 → conservative price wraps negative.
    set_mock_pyth_account(&mut svm, pyth_addr, 100, 200, -6);

    let err = try_send(
        &mut svm,
        &[ix_update_price_from_pyth(&payer.pubkey(), &mint, &pyth_addr)],
        &[&payer],
    )
    .expect_err("conf > price must reject");
    assert!(err.contains("Custom(4166)"), "expected 0x1046, got: {err}");
}

#[test]
fn update_price_from_pyth_rejects_non_oracle() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    let mint = Address::new_unique();
    send(
        &mut svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &mint, 1_000_000, 6)],
        &[&payer],
    );
    let oracle = Keypair::new();
    svm.airdrop(&oracle.pubkey(), 1_000_000_000).unwrap();
    send(
        &mut svm,
        &[ix_rotate_operator(&payer.pubkey(), role::ORACLE, &oracle.pubkey())],
        &[&payer],
    );

    let pyth_addr = Address::new_unique();
    send(
        &mut svm,
        &[ix_bind_price_feed_to_pyth(&payer.pubkey(), &mint, &pyth_addr)],
        &[&payer],
    );
    set_mock_pyth_account(&mut svm, pyth_addr, 100_000, 0, -6);

    // Payer is no longer the oracle; the rotated `oracle` key is.
    // update_price_from_pyth signed by payer must reject.
    let err = try_send(
        &mut svm,
        &[ix_update_price_from_pyth(&payer.pubkey(), &mint, &pyth_addr)],
        &[&payer],
    )
    .expect_err("non-oracle must reject");
    // UNAUTHORIZED_ORACLE = 0x1042 = 4162
    assert!(err.contains("Custom(4162)"), "expected 0x1042, got: {err}");
}

// ─── Phase 4 v1g: PythConfig owner-validation tests ─────────────────────-

fn derive_pyth_config() -> (Address, u8) {
    Address::find_program_address(&[seeds::PYTH_CONFIG], &program_id())
}

fn ix_initialize_pyth_config(
    admin: &Address,
    payer: &Address,
    pyth_program_id: &Address,
) -> Instruction {
    let (cfg, _) = derive_pyth_config();
    let mut data = Vec::with_capacity(1 + 32);
    data.push(ix::INITIALIZE_PYTH_CONFIG);
    data.extend_from_slice(pyth_program_id.as_ref());
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(derive_registry().0, false),
            AccountMeta::new(cfg, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data,
    }
}

fn ix_set_pyth_program_id(admin: &Address, new_id: &Address) -> Instruction {
    let (cfg, _) = derive_pyth_config();
    let mut data = Vec::with_capacity(1 + 32);
    data.push(ix::SET_PYTH_PROGRAM_ID);
    data.extend_from_slice(new_id.as_ref());
    Instruction {
        program_id: program_id(),
        accounts: vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new_readonly(derive_registry().0, false),
            AccountMeta::new(cfg, false),
        ],
        data,
    }
}

/// `update_price_from_pyth` with the optional `PythConfig` account
/// appended. Setting `cfg_opt = None` exercises the v1f-compat path.
fn ix_update_price_from_pyth_with_cfg(
    oracle: &Address,
    mint: &Address,
    pyth_source: &Address,
    cfg_opt: Option<&Address>,
) -> Instruction {
    let (feed, _) = derive_price_feed(mint);
    let mut accounts = vec![
        AccountMeta::new_readonly(*oracle, true),
        AccountMeta::new_readonly(derive_registry().0, false),
        AccountMeta::new(feed, false),
        AccountMeta::new_readonly(*pyth_source, false),
    ];
    if let Some(cfg) = cfg_opt {
        accounts.push(AccountMeta::new_readonly(*cfg, false));
    }
    Instruction {
        program_id: program_id(),
        accounts,
        data: vec![ix::UPDATE_PRICE_FROM_PYTH],
    }
}

#[test]
fn initialize_pyth_config_creates_pda_with_program_id() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    let pyth_pid = Address::new_unique();
    send(
        &mut svm,
        &[ix_initialize_pyth_config(&payer.pubkey(), &payer.pubkey(), &pyth_pid)],
        &[&payer],
    );

    let (cfg_pda, expected_bump) = derive_pyth_config();
    let acc = svm.get_account(&cfg_pda).expect("pyth_config PDA exists");
    assert_eq!(acc.owner, program_id());
    assert_eq!(acc.data.len(), PythConfig::LEN);
    let cfg: &PythConfig = from_bytes(&acc.data[..PythConfig::LEN]);
    assert_eq!(cfg.pyth_program_id, pyth_pid.to_bytes());
    assert_eq!(cfg.version, PythConfig::CURRENT_VERSION);
    assert_eq!(cfg.bump, expected_bump);
}

#[test]
fn initialize_pyth_config_rejects_non_super() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);

    let imposter = Keypair::new();
    svm.airdrop(&imposter.pubkey(), 1_000_000_000).unwrap();
    let pyth_pid = Address::new_unique();
    let err = try_send(
        &mut svm,
        &[ix_initialize_pyth_config(&imposter.pubkey(), &imposter.pubkey(), &pyth_pid)],
        &[&imposter],
    )
    .expect_err("init_pyth_config must reject non-super");
    assert!(err.contains("Custom(4129)"), "expected 0x1021, got: {err}");
}

#[test]
fn set_pyth_program_id_updates_config() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    let initial = Address::new_unique();
    send(
        &mut svm,
        &[ix_initialize_pyth_config(&payer.pubkey(), &payer.pubkey(), &initial)],
        &[&payer],
    );

    let new_id = Address::new_unique();
    send(&mut svm, &[ix_set_pyth_program_id(&payer.pubkey(), &new_id)], &[&payer]);

    let (cfg_pda, _) = derive_pyth_config();
    let acc = svm.get_account(&cfg_pda).unwrap();
    let cfg: &PythConfig = from_bytes(&acc.data[..PythConfig::LEN]);
    assert_eq!(cfg.pyth_program_id, new_id.to_bytes());
}

#[test]
fn set_pyth_program_id_rejects_non_super() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    let initial = Address::new_unique();
    send(
        &mut svm,
        &[ix_initialize_pyth_config(&payer.pubkey(), &payer.pubkey(), &initial)],
        &[&payer],
    );
    let imposter = Keypair::new();
    svm.airdrop(&imposter.pubkey(), 1_000_000_000).unwrap();
    let err = try_send(
        &mut svm,
        &[ix_set_pyth_program_id(&imposter.pubkey(), &Address::new_unique())],
        &[&imposter],
    )
    .expect_err("set_pyth_program_id must reject non-super");
    assert!(err.contains("Custom(4129)"), "expected 0x1021, got: {err}");
}

#[test]
fn update_price_from_pyth_with_matching_config_owner_succeeds() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    let mint = Address::new_unique();
    send(
        &mut svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &mint, 1_000_000, 6)],
        &[&payer],
    );

    let pyth_pid = Address::new_unique();
    send(
        &mut svm,
        &[ix_initialize_pyth_config(&payer.pubkey(), &payer.pubkey(), &pyth_pid)],
        &[&payer],
    );

    let pyth_addr = Address::new_unique();
    // Mock Pyth account owned by the registered Pyth program ID.
    let acc = solana_account::Account {
        lamports: 10_000_000,
        data: mock_pyth_account_data(100_000_000, 0, -6), // $100.00, no conf, exp -6
        owner: pyth_pid,
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(pyth_addr, acc).unwrap();

    send(
        &mut svm,
        &[ix_bind_price_feed_to_pyth(&payer.pubkey(), &mint, &pyth_addr)],
        &[&payer],
    );

    let (cfg_pda, _) = derive_pyth_config();
    send(
        &mut svm,
        &[ix_update_price_from_pyth_with_cfg(&payer.pubkey(), &mint, &pyth_addr, Some(&cfg_pda))],
        &[&payer],
    );

    let (feed_pda, _) = derive_price_feed(&mint);
    let feed_acc = svm.get_account(&feed_pda).unwrap();
    let pf: &PriceFeed = from_bytes(&feed_acc.data[..PriceFeed::LEN]);
    assert_eq!(pf.price_micro_usd, 100_000_000, "$100.00 = 100_000_000 micro-USD");
}

#[test]
fn update_price_from_pyth_with_wrong_config_owner_rejects() {
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    let mint = Address::new_unique();
    send(
        &mut svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &mint, 1_000_000, 6)],
        &[&payer],
    );

    let pyth_pid = Address::new_unique();
    send(
        &mut svm,
        &[ix_initialize_pyth_config(&payer.pubkey(), &payer.pubkey(), &pyth_pid)],
        &[&payer],
    );

    let pyth_addr = Address::new_unique();
    // Mock Pyth account owned by a DIFFERENT program — v1g owner
    // check should reject.
    let imposter_owner = Address::new_unique();
    let acc = solana_account::Account {
        lamports: 10_000_000,
        data: mock_pyth_account_data(100_000_000, 0, -6),
        owner: imposter_owner,
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(pyth_addr, acc).unwrap();

    send(
        &mut svm,
        &[ix_bind_price_feed_to_pyth(&payer.pubkey(), &mint, &pyth_addr)],
        &[&payer],
    );

    let (cfg_pda, _) = derive_pyth_config();
    let err = try_send(
        &mut svm,
        &[ix_update_price_from_pyth_with_cfg(&payer.pubkey(), &mint, &pyth_addr, Some(&cfg_pda))],
        &[&payer],
    )
    .expect_err("wrong-owner pyth account must reject");
    // PYTH_PROGRAM_ID_MISMATCH = 0x104A = 4170
    assert!(err.contains("Custom(4170)"), "expected 0x104A, got: {err}");
}

#[test]
fn update_price_from_pyth_without_config_falls_back_to_v1f() {
    // Backwards-compat: when PythConfig isn't passed (legacy v1f
    // call shape), the owner check is skipped. This lets pre-v1g
    // deployments keep working until they opt into the owner gate.
    let Some((mut svm, payer)) = setup() else { return };
    send(&mut svm, &[ix_initialize_registry(&payer.pubkey())], &[&payer]);
    let mint = Address::new_unique();
    send(
        &mut svm,
        &[ix_register_price_feed(&payer.pubkey(), &payer.pubkey(), &mint, 1_000_000, 6)],
        &[&payer],
    );

    let pyth_addr = Address::new_unique();
    // Owner is arbitrary; without PythConfig the handler doesn't care.
    let acc = solana_account::Account {
        lamports: 10_000_000,
        data: mock_pyth_account_data(50_000_000, 0, -6), // $50.00
        owner: Address::new_unique(),
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(pyth_addr, acc).unwrap();

    send(
        &mut svm,
        &[ix_bind_price_feed_to_pyth(&payer.pubkey(), &mint, &pyth_addr)],
        &[&payer],
    );

    // No PythConfig in trailing accounts → v1f path.
    send(
        &mut svm,
        &[ix_update_price_from_pyth_with_cfg(&payer.pubkey(), &mint, &pyth_addr, None)],
        &[&payer],
    );

    let (feed_pda, _) = derive_price_feed(&mint);
    let feed_acc = svm.get_account(&feed_pda).unwrap();
    let pf: &PriceFeed = from_bytes(&feed_acc.data[..PriceFeed::LEN]);
    assert_eq!(pf.price_micro_usd, 50_000_000);
}
