//! `ssr-compliance` — on-chain KYC/AML/accredited registry for SSR.
//!
//! Maintains per-participant compliance records (PDA per wallet) and
//! exposes a Token-2022 transfer-hook–compatible check entrypoint. Every
//! other SSR program (issuance, vault, margin) treats this program as the
//! single source of truth for "may participant X hold / transfer / borrow
//! this asset?".
//!
//! Phase 0 ships:
//!   * Instruction dispatch + custom error codes
//!   * `check_transfer` — full read-side enforcement (this is the hot path
//!     that runs on every Token-2022 transfer, so it lands first)
//!
//! Phase 0b will fill in `initialize_registry` / `register_account` /
//! `update_status` once the PDA derivation and admin-authority model are
//! settled with legal.

#![cfg_attr(not(test), no_std)]

use pinocchio::{
    ProgramResult, account_info::AccountInfo,
    instruction::{Seed, Signer},
    msg,
    program_error::ProgramError,
    pubkey::{Pubkey, create_program_address, find_program_address},
    sysvars::{Sysvar, clock::Clock, rent::Rent},
};
use pinocchio_system::instructions::CreateAccount;
use ssr_types::{
    AccountRecord, CheckError, PriceFeed, PythConfig, Registry, RiskParams, compliance_status,
    is_valid_status_transition, role, seeds,
};

// Program entrypoint, allocator, and panic handler. Split apart (rather
// than using `pinocchio::entrypoint!`) because we are `no_std` outside of
// `cfg(test)`, and the convenience `entrypoint!` macro emits the std
// `default_panic_handler!()` flavor — which does not actually register a
// `#[panic_handler]` and so the BPF link fails with "panic_handler
// function required, but not found". `nostd_panic_handler!` does
// register one.
pinocchio::program_entrypoint!(process_instruction);
pinocchio::default_allocator!();
pinocchio::nostd_panic_handler!();

// ─── ExtraAccountMetaList byte builder ───────────────────────────────────
//
// Token-2022 reads a fixed byte layout off the `ExtraAccountMetaList`
// PDA at `seeds::EXTRA_META_LIST ++ mint`. The layout is the SPL TLV
// format: an 8-byte type discriminator (= the hook's `Execute`
// discriminator), a 4-byte TLV length, then a `PodSlice<ExtraAccountMeta>`
// (max_length u32 + length u32 + entries).
//
// We hand-roll the bytes rather than depending on `spl-tlv-account-resolution`
// because (a) Pinocchio + no_std avoids the solana-program dep tree pulled
// in by the SPL crates, and (b) our meta set is fixed (always two PDA
// entries, source and destination `AccountRecord`s, derived via SPL
// `AccountData` seeds reading the owner field of the source/destination
// token accounts). A golden-bytes test in the test module verifies our
// hand-rolled output matches `spl-tlv-account-resolution`'s output for
// the same inputs — that's our drift safety net.

pub mod extra_metas {
    use super::hook_disc;

    /// Number of extra-account metas SSR declares to Token-2022.
    pub const NUM_METAS: usize = 2;
    /// Byte size of a single `ExtraAccountMeta` entry.
    pub const META_LEN: usize = 35;
    /// Byte size of the `PodSlice<ExtraAccountMeta>` header that wraps
    /// the entries. Per `spl-pod::slice::PodSlice`, this is a single
    /// `length: u32` — there is no separate `max_length` field.
    pub const POD_SLICE_HEADER: usize = 4;
    /// Byte size of the outer TLV header (`type: [u8;8]` + `length: u32`).
    pub const TLV_HEADER: usize = 12;
    /// Total bytes the account needs to hold our meta list.
    pub const ACCOUNT_SIZE: usize = TLV_HEADER + POD_SLICE_HEADER + META_LEN * NUM_METAS;

    /// Discriminator byte inside an `ExtraAccountMeta` indicating the
    /// `address_config` encodes packed seeds (rather than a literal
    /// pubkey).
    const META_DISCRIMINATOR_PDA: u8 = 1;
    /// Seed-config discriminator for `Seed::Literal { bytes }`.
    const SEED_LITERAL: u8 = 1;
    /// Seed-config discriminator for `Seed::AccountData { account_index, data_index, length }`.
    const SEED_ACCOUNT_DATA: u8 = 4;

    /// The literal seed string used in the `AccountRecord` PDA derivation.
    const RECORD_SEED: &[u8] = ssr_types::seeds::ACCOUNT_RECORD;

    /// In the SPL token account layout, the `owner` field starts at
    /// offset 32 and is 32 bytes long. The compliance gate derives the
    /// `AccountRecord` PDA from the owner pubkey, so we instruct
    /// Token-2022 to read that range out of the source / destination
    /// token accounts when resolving our extras.
    const SPL_TOKEN_OWNER_OFFSET: u8 = 32;
    const PUBKEY_LEN: u8 = 32;

    /// Build the full byte buffer Token-2022 will read out of the
    /// `ExtraAccountMetaList` PDA. `out.len()` must be exactly
    /// `ACCOUNT_SIZE`.
    pub fn write(out: &mut [u8]) -> Result<(), pinocchio::program_error::ProgramError> {
        if out.len() != ACCOUNT_SIZE {
            return Err(pinocchio::program_error::ProgramError::AccountDataTooSmall);
        }
        for b in out.iter_mut() {
            *b = 0;
        }
        // TLV type discriminator = the hook's `Execute` discriminator.
        out[0..8].copy_from_slice(&hook_disc::EXECUTE);
        // TLV length = PodSlice header (4) + entries.
        let tlv_value_size = (POD_SLICE_HEADER + META_LEN * NUM_METAS) as u32;
        out[8..12].copy_from_slice(&tlv_value_size.to_le_bytes());
        // PodSlice length.
        out[12..16].copy_from_slice(&(NUM_METAS as u32).to_le_bytes());
        // Meta 0: source AccountRecord — derived from source token
        // account (index 0)'s owner field.
        let meta0_start = TLV_HEADER + POD_SLICE_HEADER;
        write_record_meta(&mut out[meta0_start..meta0_start + META_LEN], 0);
        // Meta 1: destination AccountRecord — derived from destination
        // token account (index 2)'s owner field.
        let meta1_start = meta0_start + META_LEN;
        write_record_meta(&mut out[meta1_start..meta1_start + META_LEN], 2);
        Ok(())
    }

    /// Write one `ExtraAccountMeta` entry whose PDA seeds are
    /// `[Literal "account_record", AccountData(token_account_index, 32, 32)]`.
    fn write_record_meta(meta: &mut [u8], token_account_index: u8) {
        debug_assert_eq!(meta.len(), META_LEN);
        meta[0] = META_DISCRIMINATOR_PDA;
        let cfg = &mut meta[1..33]; // 32-byte address_config
        // Seed 0: Literal "account_record" (1 + 1 + 14 = 16 bytes)
        cfg[0] = SEED_LITERAL;
        cfg[1] = RECORD_SEED.len() as u8;
        cfg[2..2 + RECORD_SEED.len()].copy_from_slice(RECORD_SEED);
        let next = 2 + RECORD_SEED.len();
        // Seed 1: AccountData { account_index, data_index: 32, length: 32 } (4 bytes)
        cfg[next] = SEED_ACCOUNT_DATA;
        cfg[next + 1] = token_account_index;
        cfg[next + 2] = SPL_TOKEN_OWNER_OFFSET;
        cfg[next + 3] = PUBKEY_LEN;
        // Remaining cfg bytes are zero (uninitialized — the SPL unpack
        // loop stops on a 0 discriminator).
        meta[33] = 0; // is_signer = false
        meta[34] = 0; // is_writable = false
    }
}

// ─── Instruction discriminators ──────────────────────────────────────────

/// First byte of `instruction_data` — selects which handler runs for the
/// internal admin instructions. Token-2022 calls into us via a separate
/// 8-byte SPL discriminator (`hook_disc::*`); the dispatcher checks for
/// those first so the two namespaces don't collide.
pub mod ix {
    /// Create the global registry account (admin authority, version flag).
    pub const INITIALIZE_REGISTRY: u8 = 0;
    /// Onboard a new participant in `PENDING` status.
    pub const REGISTER_ACCOUNT: u8 = 1;
    /// Admin authority updates a participant's status / flags.
    pub const UPDATE_STATUS: u8 = 2;
    /// Token-2022 transfer-hook entrypoint, internal-call form.
    /// Production callers (Token-2022) come in via `hook_disc::EXECUTE`;
    /// this 1-byte tag is kept for admin/regression testing.
    pub const CHECK_TRANSFER: u8 = 3;
    /// Super-admin rotates an operational role to a new pubkey.
    pub const ROTATE_OPERATORS: u8 = 4;
    /// Super-admin allocates and seeds the global `RiskParams` PDA
    /// (Phase 4 v1c — governance-mutable haircut table).
    pub const INITIALIZE_RISK_PARAMS: u8 = 5;
    /// Super-admin updates one cell of `RiskParams::haircut_bps`.
    pub const SET_HAIRCUT: u8 = 6;
    /// Super-admin allocates a `PriceFeed` PDA for a mint
    /// (Phase 4 v1d — oracle-priced cross-margin).
    pub const REGISTER_PRICE_FEED: u8 = 7;
    /// Oracle operator writes a fresh price to a `PriceFeed` PDA.
    pub const UPDATE_PRICE: u8 = 8;
    /// Super-admin updates `RiskParams::max_staleness_slots`.
    pub const SET_MAX_STALENESS: u8 = 9;
    /// Super-admin binds a `PriceFeed` PDA to a Pyth
    /// `PriceUpdateV2` account (Phase 4 v1f — oracle adapter).
    pub const BIND_PRICE_FEED_TO_PYTH: u8 = 10;
    /// Oracle operator refreshes a Pyth-bound `PriceFeed` from the
    /// live Pyth account.
    pub const UPDATE_PRICE_FROM_PYTH: u8 = 11;
    /// Super-admin allocates the global `PythConfig` PDA, enabling
    /// owner-validation on every Pyth bind / update (Phase 4 v1g).
    pub const INITIALIZE_PYTH_CONFIG: u8 = 12;
    /// Super-admin updates `PythConfig.pyth_program_id`.
    pub const SET_PYTH_PROGRAM_ID: u8 = 13;
}

/// SPL Transfer-Hook Interface discriminators.
///
/// These are the first 8 bytes of `sha256("spl-transfer-hook-interface:<method>")`
/// per the SPL spec. Token-2022 (and any other transfer-hook caller)
/// uses these to invoke our program — our 1-byte `ix::*` tag is internal
/// only.
///
/// Constants are verified at test time against a fresh sha256 computation
/// in `tests::spl_discriminators_match_sha256`; if SPL ever changes the
/// namespace string, the test will fail loudly rather than silently
/// passing wrong bytes to a deployed program.
pub mod hook_disc {
    /// `sha256("spl-transfer-hook-interface:execute")[..8]`
    pub const EXECUTE: [u8; 8] = [105, 37, 101, 197, 75, 251, 102, 26];
    /// `sha256("spl-transfer-hook-interface:initialize-extra-account-metas")[..8]`
    pub const INITIALIZE_EXTRA_ACCOUNT_METAS: [u8; 8] = [43, 34, 13, 49, 167, 88, 235, 235];
    /// Length of an SPL discriminator prefix.
    pub const LEN: usize = 8;
}

/// Bit positions in `update_status`'s `change_mask` byte.
pub mod change_mask {
    /// Update `AccountRecord::status`.
    pub const STATUS: u8 = 0b0000_0001;
    /// Update `AccountRecord::flags` (replaces, not merges).
    pub const FLAGS: u8 = 0b0000_0010;
}

// ─── Custom error codes ──────────────────────────────────────────────────
//
// All compliance-program errors live in the 0x1000–0x1FFF range so they
// don't collide with other SSR programs once those land. Off-chain SDKs
// surface the symbol names to operators.

pub mod err {
    use pinocchio::program_error::ProgramError;

    /// Account record exists but participant is not in `VERIFIED` status.
    pub const TRANSFER_DENIED_UNVERIFIED: ProgramError = ProgramError::Custom(0x1001);
    /// Participant is under temporary suspension.
    pub const TRANSFER_DENIED_SUSPENDED: ProgramError = ProgramError::Custom(0x1002);
    /// Participant is permanently blocked.
    pub const TRANSFER_DENIED_BLOCKED: ProgramError = ProgramError::Custom(0x1003);
    /// Record account data is shorter than `AccountRecord::LEN` or
    /// misaligned for Pod cast.
    pub const RECORD_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x1010);
    /// Record's status byte is outside the recognized discriminant range.
    /// Programs reject rather than silently treating unknowns as "unverified".
    pub const RECORD_STATUS_UNKNOWN: ProgramError = ProgramError::Custom(0x1011);
    /// Caller's signer pubkey doesn't match `Registry::status_operator`
    /// (or `Registry::onboard_operator`, depending on the handler).
    pub const UNAUTHORIZED_OPERATOR: ProgramError = ProgramError::Custom(0x1020);
    /// Caller's signer pubkey doesn't match `Registry::super_admin`.
    /// Distinct from `UNAUTHORIZED_OPERATOR` because rotation incidents
    /// are higher-severity than routine status-update failures.
    pub const UNAUTHORIZED_SUPER_ADMIN: ProgramError = ProgramError::Custom(0x1021);
    /// Proposed `(from, to)` status transition is rejected by the
    /// policy in `ssr_types::is_valid_status_transition` (e.g., trying
    /// to revive a `BLOCKED` account, or a no-op self-transition).
    pub const INVALID_STATUS_TRANSITION: ProgramError = ProgramError::Custom(0x1022);
    /// `role` byte in `rotate_operators` is outside `role::is_known`.
    pub const UNKNOWN_ROLE: ProgramError = ProgramError::Custom(0x1023);
    /// Instruction data buffer is shorter than the handler expects.
    pub const INSTRUCTION_DATA_TOO_SHORT: ProgramError = ProgramError::Custom(0x1024);
    /// The registry account passed in does not match the PDA derived
    /// from the cached `bump` + `seeds::REGISTRY`. Either the wrong
    /// account was passed, or the registry's stored bump is corrupt.
    pub const REGISTRY_PDA_MISMATCH: ProgramError = ProgramError::Custom(0x1025);
    /// The account-record account passed in does not match the PDA
    /// derived from the cached `bump` + `seeds::ACCOUNT_RECORD` + the
    /// record's own `participant` field.
    pub const RECORD_PDA_MISMATCH: ProgramError = ProgramError::Custom(0x1026);
    /// A signer was expected but the account is not marked as a signer.
    pub const MISSING_SIGNER: ProgramError = ProgramError::Custom(0x1027);
    /// `ExtraAccountMetaList` PDA passed in does not match the address
    /// derived from `[b"extra-account-metas", mint]`.
    pub const META_LIST_PDA_MISMATCH: ProgramError = ProgramError::Custom(0x1028);
    /// Mint account is shorter than the base 36-byte
    /// `mint_authority: COption<Pubkey>` field.
    pub const MINT_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x1029);
    /// Mint's `mint_authority` is `None` — issuance frozen, meta-list
    /// init must be rejected so the configuration cannot be set by an
    /// arbitrary subsequent caller.
    pub const MINT_AUTHORITY_NONE: ProgramError = ProgramError::Custom(0x102A);
    /// Mint authority signer does not match the `mint_authority` field
    /// recorded in the mint account data.
    pub const MINT_AUTHORITY_MISMATCH: ProgramError = ProgramError::Custom(0x102B);

    // Phase 4 v1c — RiskParams governance.
    /// `RiskParams` PDA passed in does not match
    /// `[seeds::RISK_PARAMS] @ program_id`.
    pub const RISK_PARAMS_PDA_MISMATCH: ProgramError = ProgramError::Custom(0x1030);
    /// `RiskParams` account data is shorter than `RiskParams::LEN` or
    /// misaligned for Pod cast.
    pub const RISK_PARAMS_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x1031);
    /// `set_haircut`'s `class` byte is `>= RiskParams::HAIRCUT_TABLE_LEN`
    /// — the table can't index that asset class.
    pub const ASSET_CLASS_OUT_OF_RANGE: ProgramError = ProgramError::Custom(0x1032);
    /// `set_haircut`'s `bps` value is `> 10_000` — a haircut over 100%
    /// has no defensible meaning.
    pub const HAIRCUT_OUT_OF_RANGE: ProgramError = ProgramError::Custom(0x1033);

    // Phase 4 v1d — oracle-priced cross-margin.
    /// `PriceFeed` PDA passed in does not derive from
    /// `[seeds::PRICE_FEED, mint] @ program_id`.
    pub const PRICE_FEED_PDA_MISMATCH: ProgramError = ProgramError::Custom(0x1040);
    /// `PriceFeed` account data is shorter than `PriceFeed::LEN` or
    /// misaligned for Pod cast.
    pub const PRICE_FEED_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x1041);
    /// Caller's signer pubkey doesn't match `Registry::oracle_operator`.
    /// Distinct from `UNAUTHORIZED_OPERATOR` so audit logs can tell
    /// "price feed tampering attempt" apart from "compliance op
    /// tampering attempt".
    pub const UNAUTHORIZED_ORACLE: ProgramError = ProgramError::Custom(0x1042);

    // Phase 4 v1f — Pyth adapter.
    /// `update_price_from_pyth` invoked on a feed whose `pyth_source`
    /// is still `[0; 32]`. Run `bind_price_feed_to_pyth` first.
    pub const PRICE_FEED_NOT_PYTH_BOUND: ProgramError = ProgramError::Custom(0x1043);
    /// Passed Pyth account doesn't match the feed's bound
    /// `pyth_source`.
    pub const PYTH_SOURCE_MISMATCH: ProgramError = ProgramError::Custom(0x1044);
    /// Passed Pyth account's discriminator or data length doesn't
    /// match the expected `PriceUpdateV2` layout.
    pub const PYTH_ACCOUNT_INVALID: ProgramError = ProgramError::Custom(0x1045);
    /// Pyth raw price is negative (after subtracting confidence
    /// interval). Treated as a hard error rather than clamped to 0
    /// because a negative quoted price almost always means the feed
    /// is broken / under-attack and shouldn't drive margin decisions.
    pub const PYTH_NEGATIVE_PRICE: ProgramError = ProgramError::Custom(0x1046);
    /// Pyth exponent yields a `10^N` factor outside the range
    /// `0 ≤ N ≤ 18` that can be applied without overflowing u128.
    /// Extremely uncommon for real assets (typical exponents are
    /// -2 to -9); surfaces as an explicit error rather than a
    /// silent saturation.
    pub const PYTH_EXPONENT_OUT_OF_RANGE: ProgramError = ProgramError::Custom(0x1047);

    // Phase 4 v1g — PythConfig owner-validation gate.
    /// `PythConfig` PDA passed in does not derive from
    /// `[seeds::PYTH_CONFIG] @ program_id`.
    pub const PYTH_CONFIG_PDA_MISMATCH: ProgramError = ProgramError::Custom(0x1048);
    /// `PythConfig` data length / Pod cast invalid.
    pub const PYTH_CONFIG_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x1049);
    /// Bound / passed Pyth account is not owned by
    /// `PythConfig.pyth_program_id`. The bind / update was attempted
    /// against an account that isn't a real Pyth account, or Pyth
    /// has redeployed under a new program ID — investigate then
    /// `set_pyth_program_id` to the new value.
    pub const PYTH_PROGRAM_ID_MISMATCH: ProgramError = ProgramError::Custom(0x104A);

    /// Phase 0c stub — currently unused; reserved for future deferred handlers.
    pub const NOT_IMPLEMENTED: ProgramError = ProgramError::Custom(0x1FFF);
}

// ─── Entrypoint dispatch ─────────────────────────────────────────────────

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    // SPL hook-interface dispatch comes first. Production callers
    // (Token-2022) always start with one of these 8-byte discriminators;
    // routing them before the 1-byte admin tag avoids any chance of
    // collision and keeps the hot transfer path's CU cost minimal
    // (one slice-equals on entry).
    if instruction_data.len() >= hook_disc::LEN {
        let head: [u8; 8] = instruction_data[..hook_disc::LEN].try_into().unwrap();
        if head == hook_disc::EXECUTE {
            // Token-2022 follows the 8-byte disc with `amount: u64 LE`.
            // We don't currently use the amount — gate decisions are
            // status-only — but accept the data layout for ABI compat.
            return check_transfer(accounts);
        }
        if head == hook_disc::INITIALIZE_EXTRA_ACCOUNT_METAS {
            return initialize_extra_account_meta_list(program_id, accounts);
        }
    }

    // Admin / internal-test dispatch on the 1-byte tag.
    let (tag, rest) = instruction_data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;
    match *tag {
        ix::INITIALIZE_REGISTRY => initialize_registry(program_id, accounts),
        ix::REGISTER_ACCOUNT => register_account(program_id, accounts, rest),
        ix::UPDATE_STATUS => update_status(program_id, accounts, rest),
        ix::CHECK_TRANSFER => check_transfer(accounts),
        ix::ROTATE_OPERATORS => rotate_operators(program_id, accounts, rest),
        ix::INITIALIZE_RISK_PARAMS => initialize_risk_params(program_id, accounts),
        ix::SET_HAIRCUT => set_haircut(program_id, accounts, rest),
        ix::REGISTER_PRICE_FEED => register_price_feed(program_id, accounts, rest),
        ix::UPDATE_PRICE => update_price(program_id, accounts, rest),
        ix::SET_MAX_STALENESS => set_max_staleness(program_id, accounts, rest),
        ix::BIND_PRICE_FEED_TO_PYTH => bind_price_feed_to_pyth(program_id, accounts, rest),
        ix::UPDATE_PRICE_FROM_PYTH => update_price_from_pyth(program_id, accounts),
        ix::INITIALIZE_PYTH_CONFIG => initialize_pyth_config(program_id, accounts, rest),
        ix::SET_PYTH_PROGRAM_ID => set_pyth_program_id(program_id, accounts, rest),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

// ─── Write-side handlers (Phase 0b) ──────────────────────────────────────
//
// Intentionally stubbed: the admin-authority model (single multisig vs
// tiered approvers, the slot-cadence re-screening rule, the audit-log
// emission shape) needs sign-off before we commit to a wire format. We
// return a distinct error so callers can tell "not yet" apart from
// "permanently failed".

// ─── initialize_registry ─────────────────────────────────────────────────

/// Allocate and populate the global registry PDA.
///
/// The payer becomes the initial `super_admin`. Operational role
/// pubkeys default to the same value — the caller is expected to
/// `rotate_operators` to dedicated ops pubkeys before going to
/// production (typically the first thing run after the multisig that
/// will hold super-admin authority signs off on a deployment).
///
/// Accounts:
///   [0, signer, write]  payer (becomes the initial super_admin)
///   [1, write]          registry PDA (to be created at `seeds::REGISTRY`)
///   [2, read]           system_program (for the create-account CPI)
fn initialize_registry(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let (payer_ai, registry_ai, _system_program_ai) = match accounts {
        [payer, reg, sys, ..] => (payer, reg, sys),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    require_signer(payer_ai)?;

    let (expected_pda, bump) = find_program_address(&[seeds::REGISTRY], program_id);
    if &expected_pda != registry_ai.key() {
        return Err(err::REGISTRY_PDA_MISMATCH);
    }

    // Compute the rent floor via the sysvar syscall — no need to pass the
    // rent account through the instruction.
    let lamports = Rent::get()?.minimum_balance(Registry::LEN);

    let bump_seed = [bump];
    let pda_seeds = [Seed::from(seeds::REGISTRY), Seed::from(&bump_seed[..])];
    let pda_signer = Signer::from(&pda_seeds);

    CreateAccount {
        from: payer_ai,
        to: registry_ai,
        lamports,
        space: Registry::LEN as u64,
        owner: program_id,
    }
    .invoke_signed(&[pda_signer])?;

    let slot = Clock::get()?.slot;
    let mut data = registry_ai.try_borrow_mut_data()?;
    let registry: &mut Registry = bytemuck::try_from_bytes_mut(&mut data[..Registry::LEN])
        .map_err(|_| err::REGISTRY_PDA_MISMATCH)?;
    *registry = Registry::initial(*payer_ai.key(), slot, bump);

    msg!("ssr-compliance: initialize_registry");
    Ok(())
}

// ─── register_account ────────────────────────────────────────────────────

/// Onboard a new participant. The onboard_operator signer authorizes;
/// the payer funds the new account. The participant's wallet does NOT
/// need to sign — onboarding is operator-driven, and the participant's
/// pubkey is supplied via instruction data.
///
/// Accounts:
///   [0, signer]         onboard_operator
///   [1, signer, write]  payer
///   [2, read]           registry PDA (to verify operator)
///   [3, write]          account_record PDA (to be created)
///   [4, read]           system_program
///
/// Instruction data (after the dispatch tag):
///   [0..32]   participant pubkey (32 bytes)
///   [32..34]  jurisdiction code (2 bytes)
fn register_account(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (operator_ai, payer_ai, registry_ai, record_ai, _system_program_ai) = match accounts {
        [op, p, reg, rec, sys, ..] => (op, p, reg, rec, sys),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 34 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let mut participant = [0u8; 32];
    participant.copy_from_slice(&data[0..32]);
    let mut jurisdiction = [0u8; 2];
    jurisdiction.copy_from_slice(&data[32..34]);

    require_signer(operator_ai)?;
    require_signer(payer_ai)?;
    let registry = read_registry(registry_ai, program_id)?;
    if registry.onboard_operator != *operator_ai.key() {
        return Err(err::UNAUTHORIZED_OPERATOR);
    }

    let (expected_pda, bump) =
        find_program_address(&[seeds::ACCOUNT_RECORD, &participant], program_id);
    if &expected_pda != record_ai.key() {
        return Err(err::RECORD_PDA_MISMATCH);
    }

    let lamports = Rent::get()?.minimum_balance(AccountRecord::LEN);

    let bump_seed = [bump];
    let pda_seeds = [
        Seed::from(seeds::ACCOUNT_RECORD),
        Seed::from(&participant[..]),
        Seed::from(&bump_seed[..]),
    ];
    let pda_signer = Signer::from(&pda_seeds);

    CreateAccount {
        from: payer_ai,
        to: record_ai,
        lamports,
        space: AccountRecord::LEN as u64,
        owner: program_id,
    }
    .invoke_signed(&[pda_signer])?;

    let slot = Clock::get()?.slot;
    let mut record_data = record_ai.try_borrow_mut_data()?;
    let record: &mut AccountRecord =
        bytemuck::try_from_bytes_mut(&mut record_data[..AccountRecord::LEN])
            .map_err(|_| err::RECORD_LAYOUT_INVALID)?;
    *record = AccountRecord::pending(participant, jurisdiction, slot, bump);

    msg!("ssr-compliance: register_account");
    Ok(())
}

// ─── update_status ───────────────────────────────────────────────────────

/// Update an existing `AccountRecord`. The status_operator signer
/// authorizes; the proposed transition must satisfy
/// `is_valid_status_transition` (which forbids reviving `BLOCKED`).
///
/// Accounts:
///   [0, signer]   status_operator
///   [1, read]     registry PDA (`seeds::REGISTRY`)
///   [2, write]    account_record PDA (`seeds::ACCOUNT_RECORD ++ participant`)
///
/// Instruction data (after the dispatch tag):
///   [0]  new_status
///   [1]  new_flags
///   [2]  change_mask  — bits in `change_mask::{STATUS, FLAGS}`
fn update_status(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (operator_ai, registry_ai, record_ai) = match accounts {
        [op, reg, rec, ..] => (op, reg, rec),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 3 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let new_status = data[0];
    let new_flags = data[1];
    let mask = data[2];

    require_signer(operator_ai)?;
    let registry = read_registry(registry_ai, program_id)?;
    if registry.status_operator != *operator_ai.key() {
        return Err(err::UNAUTHORIZED_OPERATOR);
    }

    // Borrow the record mutably and apply the transition in-place. We
    // hold the mutable borrow for the full operation so the audit log
    // emission sees the post-mutation state.
    let mut record_data = record_ai.try_borrow_mut_data()?;
    if record_data.len() < AccountRecord::LEN {
        return Err(err::RECORD_LAYOUT_INVALID);
    }
    let record: &mut AccountRecord =
        bytemuck::try_from_bytes_mut(&mut record_data[..AccountRecord::LEN])
            .map_err(|_| err::RECORD_LAYOUT_INVALID)?;
    verify_record_pda(record_ai, record, program_id)?;

    let slot = Clock::get()?.slot;

    if (mask & change_mask::STATUS) != 0 {
        if !is_valid_status_transition(record.status, new_status) {
            return Err(err::INVALID_STATUS_TRANSITION);
        }
        record.status = new_status;
        record.updated_at_slot = slot;
    }
    if (mask & change_mask::FLAGS) != 0 {
        record.flags = new_flags;
        // updated_at_slot is bumped if either field changes
        record.updated_at_slot = slot;
    }

    msg!("ssr-compliance: update_status");
    Ok(())
}

// ─── rotate_operators ────────────────────────────────────────────────────

/// Super-admin rotates one of the operational role pubkeys.
///
/// Accounts:
///   [0, signer]   super_admin
///   [1, write]    registry PDA
///
/// Instruction data (after the dispatch tag):
///   [0]      role     — `role::ONBOARD` or `role::STATUS`
///   [1..33]  new_pubkey (32 bytes)
fn rotate_operators(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (admin_ai, registry_ai) = match accounts {
        [adm, reg, ..] => (adm, reg),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 33 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let target_role = data[0];
    let mut new_pubkey = [0u8; 32];
    new_pubkey.copy_from_slice(&data[1..33]);

    require_signer(admin_ai)?;
    if !role::is_known(target_role) {
        return Err(err::UNKNOWN_ROLE);
    }

    let mut registry_data = registry_ai.try_borrow_mut_data()?;
    if registry_data.len() < Registry::LEN {
        return Err(err::REGISTRY_PDA_MISMATCH);
    }
    let registry: &mut Registry =
        bytemuck::try_from_bytes_mut(&mut registry_data[..Registry::LEN])
            .map_err(|_| err::REGISTRY_PDA_MISMATCH)?;
    verify_registry_pda(registry_ai, registry, program_id)?;
    if registry.super_admin != *admin_ai.key() {
        return Err(err::UNAUTHORIZED_SUPER_ADMIN);
    }

    match target_role {
        role::ONBOARD => registry.onboard_operator = new_pubkey,
        role::STATUS => registry.status_operator = new_pubkey,
        role::ORACLE => registry.oracle_operator = new_pubkey,
        _ => unreachable!("guarded by role::is_known above"),
    }
    registry.last_modified_slot = Clock::get()?.slot;

    msg!("ssr-compliance: rotate_operators");
    Ok(())
}

// ─── initialize_risk_params (Phase 4 v1c) ───────────────────────────────-

/// Allocate the global `RiskParams` PDA and seed it with
/// `DEFAULT_HAIRCUTS`. Idempotency: callers must not invoke this
/// twice — the system program's `CreateAccount` will fail with
/// `AccountAlreadyInitialized` when the PDA already holds lamports.
///
/// Authority: super-admin (matches the value in `Registry::super_admin`).
/// We *don't* let the payer become the implicit super-admin like
/// `initialize_registry` does, because RiskParams is allocated after
/// the registry is already bootstrapped — the source of truth for
/// "who is super-admin" already exists.
///
/// Accounts:
///   [0, signer]         super_admin
///   [1, signer, write]  payer (funds the PDA's rent)
///   [2, read]           registry PDA (to verify super_admin)
///   [3, write]          risk_params PDA (to be created at `seeds::RISK_PARAMS`)
///   [4, read]           system_program
fn initialize_risk_params(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let (admin_ai, payer_ai, registry_ai, risk_ai, _system_ai) = match accounts {
        [adm, pay, reg, risk, sys, ..] => (adm, pay, reg, risk, sys),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    require_signer(admin_ai)?;
    require_signer(payer_ai)?;

    let registry = read_registry(registry_ai, program_id)?;
    if registry.super_admin != *admin_ai.key() {
        return Err(err::UNAUTHORIZED_SUPER_ADMIN);
    }

    let (expected_pda, bump) = find_program_address(&[seeds::RISK_PARAMS], program_id);
    if &expected_pda != risk_ai.key() {
        return Err(err::RISK_PARAMS_PDA_MISMATCH);
    }

    let lamports = Rent::get()?.minimum_balance(RiskParams::LEN);
    let bump_seed = [bump];
    let pda_seeds = [Seed::from(seeds::RISK_PARAMS), Seed::from(&bump_seed[..])];
    let pda_signer = Signer::from(&pda_seeds);

    CreateAccount {
        from: payer_ai,
        to: risk_ai,
        lamports,
        space: RiskParams::LEN as u64,
        owner: program_id,
    }
    .invoke_signed(&[pda_signer])?;

    let slot = Clock::get()?.slot;
    let mut data = risk_ai.try_borrow_mut_data()?;
    let rp: &mut RiskParams = bytemuck::try_from_bytes_mut(&mut data[..RiskParams::LEN])
        .map_err(|_| err::RISK_PARAMS_LAYOUT_INVALID)?;
    *rp = RiskParams::initial(slot, bump);

    msg!("ssr-compliance: initialize_risk_params");
    Ok(())
}

// ─── set_haircut (Phase 4 v1c) ──────────────────────────────────────────-

/// Super-admin updates one cell of `RiskParams::haircut_bps`.
///
/// Authority + PDA mismatch / data-shape rules mirror `rotate_operators`.
///
/// Accounts:
///   [0, signer]   super_admin
///   [1, read]     registry PDA (to verify super_admin)
///   [2, write]    risk_params PDA
///
/// Instruction data (after dispatch tag):
///   [0]      class — asset_class discriminant (must be < HAIRCUT_TABLE_LEN)
///   [1..3]   bps   — new haircut, u16 LE (must be <= 10_000)
fn set_haircut(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let (admin_ai, registry_ai, risk_ai) = match accounts {
        [adm, reg, risk, ..] => (adm, reg, risk),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 3 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let class = data[0];
    let bps = u16::from_le_bytes([data[1], data[2]]);
    if (class as usize) >= RiskParams::HAIRCUT_TABLE_LEN {
        return Err(err::ASSET_CLASS_OUT_OF_RANGE);
    }
    if bps > 10_000 {
        return Err(err::HAIRCUT_OUT_OF_RANGE);
    }

    require_signer(admin_ai)?;
    let registry = read_registry(registry_ai, program_id)?;
    if registry.super_admin != *admin_ai.key() {
        return Err(err::UNAUTHORIZED_SUPER_ADMIN);
    }

    let mut risk_data = risk_ai.try_borrow_mut_data()?;
    if risk_data.len() < RiskParams::LEN {
        return Err(err::RISK_PARAMS_PDA_MISMATCH);
    }
    let rp: &mut RiskParams = bytemuck::try_from_bytes_mut(&mut risk_data[..RiskParams::LEN])
        .map_err(|_| err::RISK_PARAMS_LAYOUT_INVALID)?;
    verify_risk_params_pda(risk_ai, rp, program_id)?;

    rp.haircut_bps[class as usize] = bps;
    rp.last_modified_slot = Clock::get()?.slot;

    msg!("ssr-compliance: set_haircut");
    Ok(())
}

fn verify_risk_params_pda(
    ai: &AccountInfo,
    rp: &RiskParams,
    program_id: &Pubkey,
) -> ProgramResult {
    let expected = create_program_address(&[seeds::RISK_PARAMS, &[rp.bump]], program_id)
        .map_err(|_| err::RISK_PARAMS_PDA_MISMATCH)?;
    if &expected != ai.key() {
        return Err(err::RISK_PARAMS_PDA_MISMATCH);
    }
    Ok(())
}

// ─── register_price_feed (Phase 4 v1d) ──────────────────────────────────-

/// Allocate a `PriceFeed` PDA for `mint` and seed it with an initial
/// price + mint decimals.
///
/// Authority: super-admin. Registration is a deployment-time act
/// (an asset coming under SSR's risk umbrella); price updates are
/// the cadence concern handled by `update_price` under a separate
/// `oracle_operator` role.
///
/// Accounts:
///   [0, signer]         super_admin
///   [1, signer, write]  payer
///   [2, read]           registry PDA
///   [3, write]          price_feed PDA (created at `[PRICE_FEED, mint]`)
///   [4, read]           system_program
///
/// Instruction data (after dispatch tag):
///   [0..32]   mint pubkey
///   [32..40]  initial price (u64 LE, micro-USD per native unit)
///   [40]      mint_decimals (u8)
fn register_price_feed(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (admin_ai, payer_ai, registry_ai, feed_ai, _system_ai) = match accounts {
        [adm, pay, reg, feed, sys, ..] => (adm, pay, reg, feed, sys),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 41 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let mut mint = [0u8; 32];
    mint.copy_from_slice(&data[0..32]);
    let initial_price = u64::from_le_bytes(data[32..40].try_into().unwrap());
    let mint_decimals = data[40];

    require_signer(admin_ai)?;
    require_signer(payer_ai)?;
    let registry = read_registry(registry_ai, program_id)?;
    if registry.super_admin != *admin_ai.key() {
        return Err(err::UNAUTHORIZED_SUPER_ADMIN);
    }

    let (expected_pda, bump) = find_program_address(&[seeds::PRICE_FEED, &mint], program_id);
    if &expected_pda != feed_ai.key() {
        return Err(err::PRICE_FEED_PDA_MISMATCH);
    }

    let lamports = Rent::get()?.minimum_balance(PriceFeed::LEN);
    let bump_seed = [bump];
    let pda_seeds = [
        Seed::from(seeds::PRICE_FEED),
        Seed::from(&mint[..]),
        Seed::from(&bump_seed[..]),
    ];
    let pda_signer = Signer::from(&pda_seeds);
    CreateAccount {
        from: payer_ai,
        to: feed_ai,
        lamports,
        space: PriceFeed::LEN as u64,
        owner: program_id,
    }
    .invoke_signed(&[pda_signer])?;

    let slot = Clock::get()?.slot;
    let mut data = feed_ai.try_borrow_mut_data()?;
    let pf: &mut PriceFeed = bytemuck::try_from_bytes_mut(&mut data[..PriceFeed::LEN])
        .map_err(|_| err::PRICE_FEED_LAYOUT_INVALID)?;
    *pf = PriceFeed::initial(mint, initial_price, mint_decimals, slot, bump);

    msg!("ssr-compliance: register_price_feed");
    Ok(())
}

// ─── update_price (Phase 4 v1d) ─────────────────────────────────────────-

/// Oracle operator writes a fresh price + slot to a `PriceFeed`.
/// `mint_decimals` is fixed at registration time and not touched here
/// — a decimals change is effectively a different mint and would need
/// a fresh `register_price_feed`.
///
/// Accounts:
///   [0, signer]   oracle_operator
///   [1, read]     registry PDA
///   [2, write]    price_feed PDA
///
/// Instruction data (after dispatch tag):
///   [0..8]   new_price: u64 LE (micro-USD per native unit)
fn update_price(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let (oracle_ai, registry_ai, feed_ai) = match accounts {
        [o, r, f, ..] => (o, r, f),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 8 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let new_price = u64::from_le_bytes(data[0..8].try_into().unwrap());

    require_signer(oracle_ai)?;
    let registry = read_registry(registry_ai, program_id)?;
    if registry.oracle_operator != *oracle_ai.key() {
        return Err(err::UNAUTHORIZED_ORACLE);
    }

    let mut feed_data = feed_ai.try_borrow_mut_data()?;
    if feed_data.len() < PriceFeed::LEN {
        return Err(err::PRICE_FEED_PDA_MISMATCH);
    }
    let pf: &mut PriceFeed = bytemuck::try_from_bytes_mut(&mut feed_data[..PriceFeed::LEN])
        .map_err(|_| err::PRICE_FEED_LAYOUT_INVALID)?;
    let expected = create_program_address(
        &[seeds::PRICE_FEED, &pf.mint, &[pf.bump]],
        program_id,
    )
    .map_err(|_| err::PRICE_FEED_PDA_MISMATCH)?;
    if &expected != feed_ai.key() {
        return Err(err::PRICE_FEED_PDA_MISMATCH);
    }

    pf.price_micro_usd = new_price;
    pf.last_updated_slot = Clock::get()?.slot;

    msg!("ssr-compliance: update_price");
    Ok(())
}

// ─── set_max_staleness (Phase 4 v1d) ────────────────────────────────────-

/// Super-admin updates the staleness gate on `RiskParams`.
///
/// Accounts:
///   [0, signer]   super_admin
///   [1, read]     registry PDA
///   [2, write]    risk_params PDA
///
/// Instruction data (after dispatch tag):
///   [0..8]   max_staleness_slots: u64 LE (0 disables the gate)
fn set_max_staleness(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (admin_ai, registry_ai, risk_ai) = match accounts {
        [a, r, rp, ..] => (a, r, rp),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 8 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let new_max = u64::from_le_bytes(data[0..8].try_into().unwrap());

    require_signer(admin_ai)?;
    let registry = read_registry(registry_ai, program_id)?;
    if registry.super_admin != *admin_ai.key() {
        return Err(err::UNAUTHORIZED_SUPER_ADMIN);
    }

    let mut risk_data = risk_ai.try_borrow_mut_data()?;
    if risk_data.len() < RiskParams::LEN {
        return Err(err::RISK_PARAMS_PDA_MISMATCH);
    }
    let rp: &mut RiskParams = bytemuck::try_from_bytes_mut(&mut risk_data[..RiskParams::LEN])
        .map_err(|_| err::RISK_PARAMS_LAYOUT_INVALID)?;
    verify_risk_params_pda(risk_ai, rp, program_id)?;

    rp.max_staleness_slots = new_max;
    rp.last_modified_slot = Clock::get()?.slot;

    msg!("ssr-compliance: set_max_staleness");
    Ok(())
}

// ─── bind_price_feed_to_pyth (Phase 4 v1f) ──────────────────────────────-

/// Super-admin sets a `PriceFeed.pyth_source` to a specific Pyth
/// `PriceUpdateV2` account, enabling `update_price_from_pyth` for
/// that feed. Passing `[0; 32]` as the source resets the feed to
/// manual-only mode (`update_price_from_pyth` will reject with
/// `PRICE_FEED_NOT_PYTH_BOUND`).
///
/// Trust model: we don't validate the source's owner program ID
/// against Pyth's deployed program. The super-admin is the trust
/// point — they're responsible for binding to a real Pyth account
/// at registration. Once bound, `update_price_from_pyth` only
/// accepts the exact account that was bound.
///
/// Accounts:
///   [0, signer]   super_admin
///   [1, read]     registry PDA
///   [2, write]    price_feed PDA
///   [3, read]     pyth_config PDA (optional — v1g owner-validation)
///   [4, read]     pyth account being bound (optional — v1g owner check)
///
/// When account [3] is present, the handler reads `PythConfig.pyth_program_id`
/// and verifies that account [4]'s owner matches. If either account
/// is omitted, the owner check is skipped (v1f behavior).
///
/// Instruction data (after dispatch tag):
///   [0..32]   pyth_source pubkey (zero to unbind)
fn bind_price_feed_to_pyth(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (admin_ai, registry_ai, feed_ai) = match accounts {
        [a, r, f, ..] => (a, r, f),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    let pyth_config_ai = accounts.get(3);
    let pyth_account_ai = accounts.get(4);
    if data.len() < 32 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let mut new_source = [0u8; 32];
    new_source.copy_from_slice(&data[0..32]);

    require_signer(admin_ai)?;
    let registry = read_registry(registry_ai, program_id)?;
    if registry.super_admin != *admin_ai.key() {
        return Err(err::UNAUTHORIZED_SUPER_ADMIN);
    }

    // v1g: if a PythConfig is passed, validate the bound account's
    // owner. Unbind (`new_source = [0; 32]`) skips the check — that's
    // a "disable" operation that shouldn't require a valid Pyth
    // account.
    if new_source != [0u8; 32] {
        if let (Some(cfg_ai), Some(pyth_ai)) = (pyth_config_ai, pyth_account_ai) {
            let expected_program = read_pyth_config_program_id(cfg_ai, program_id)?;
            if pyth_ai.key() != &new_source {
                // Caller passed the wrong Pyth account for verification.
                return Err(err::PYTH_SOURCE_MISMATCH);
            }
            if pyth_ai.owner() != &expected_program {
                return Err(err::PYTH_PROGRAM_ID_MISMATCH);
            }
        }
    }

    let mut feed_data = feed_ai.try_borrow_mut_data()?;
    if feed_data.len() < PriceFeed::LEN {
        return Err(err::PRICE_FEED_PDA_MISMATCH);
    }
    let pf: &mut PriceFeed = bytemuck::try_from_bytes_mut(&mut feed_data[..PriceFeed::LEN])
        .map_err(|_| err::PRICE_FEED_LAYOUT_INVALID)?;
    let expected = create_program_address(&[seeds::PRICE_FEED, &pf.mint, &[pf.bump]], program_id)
        .map_err(|_| err::PRICE_FEED_PDA_MISMATCH)?;
    if &expected != feed_ai.key() {
        return Err(err::PRICE_FEED_PDA_MISMATCH);
    }

    pf.pyth_source = new_source;
    pf.last_updated_slot = Clock::get()?.slot;

    msg!("ssr-compliance: bind_price_feed_to_pyth");
    Ok(())
}

/// Read `PythConfig.pyth_program_id` from a passed account, after
/// validating the PDA derivation. Used by v1g owner-validation in
/// bind / update flows.
fn read_pyth_config_program_id(
    cfg_ai: &AccountInfo,
    program_id: &Pubkey,
) -> Result<Pubkey, ProgramError> {
    let data = cfg_ai.try_borrow_data()?;
    if data.len() < PythConfig::LEN {
        return Err(err::PYTH_CONFIG_LAYOUT_INVALID);
    }
    let cfg: &PythConfig = bytemuck::try_from_bytes(&data[..PythConfig::LEN])
        .map_err(|_| err::PYTH_CONFIG_LAYOUT_INVALID)?;
    let expected = create_program_address(&[seeds::PYTH_CONFIG, &[cfg.bump]], program_id)
        .map_err(|_| err::PYTH_CONFIG_PDA_MISMATCH)?;
    if &expected != cfg_ai.key() {
        return Err(err::PYTH_CONFIG_PDA_MISMATCH);
    }
    Ok(cfg.pyth_program_id)
}

// ─── update_price_from_pyth (Phase 4 v1f) ───────────────────────────────-

// Anchor account discriminator for Pyth's `PriceUpdateV2`, computed
// as `sha256("account:PriceUpdateV2")[..8]`. If Pyth migrates to
// `PriceUpdateV3` (or any other layout) this constant + the parser
// below must be revised — the e2e test `pyth_discriminator_matches`
// recomputes it from the same string and asserts equality.
const PYTH_PRICE_UPDATE_V2_DISCRIMINATOR: [u8; 8] =
    [34, 241, 35, 99, 157, 126, 244, 205];

// `PriceUpdateV2` packed Anchor layout (from
// pyth-solana-receiver-sdk-0.3.2):
//   [  0..  8] discriminator
//   [  8.. 40] write_authority: Pubkey
//   [ 40.. 42] verification_level (1-byte tag + 1-byte payload)
//   [ 42.. 74] feed_id: [u8; 32]
//   [ 74.. 82] price: i64 LE
//   [ 82.. 90] conf:  u64 LE
//   [ 90.. 94] exponent: i32 LE
//   [ 94..102] publish_time: i64 LE
//   [102..110] prev_publish_time: i64 LE
//   [110..118] ema_price: i64 LE
//   [118..126] ema_conf: u64 LE
//   [126..134] posted_slot: u64 LE
const PYTH_PRICE_UPDATE_V2_LEN: usize = 134;
const PYTH_OFFSET_PRICE: usize = 74;
const PYTH_OFFSET_CONF: usize = 82;
const PYTH_OFFSET_EXPONENT: usize = 90;

/// Oracle operator refreshes a Pyth-bound `PriceFeed`. The handler
/// reads the bound Pyth account, applies the confidence interval
/// (`price - conf` as the conservative quote), normalizes the
/// exponent to micro-USD, and writes the result into the cached
/// `price_micro_usd` + bumps `last_updated_slot`. The downstream
/// `RiskParams.max_staleness_slots` gate then enforces freshness at
/// margin-check time.
///
/// Accounts:
///   [0, signer]   oracle_operator
///   [1, read]     registry PDA
///   [2, write]    price_feed PDA
///   [3, read]     pyth account (must equal price_feed.pyth_source)
///   [4, read]     pyth_config PDA (optional — v1g owner-validation)
///
/// When account [4] is present, the handler verifies the passed Pyth
/// account's owner matches `PythConfig.pyth_program_id`. Skipped if
/// omitted (v1f behavior).
fn update_price_from_pyth(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let (oracle_ai, registry_ai, feed_ai, pyth_ai) = match accounts {
        [o, r, f, p, ..] => (o, r, f, p),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    let pyth_config_ai = accounts.get(4);

    require_signer(oracle_ai)?;
    let registry = read_registry(registry_ai, program_id)?;
    if registry.oracle_operator != *oracle_ai.key() {
        return Err(err::UNAUTHORIZED_ORACLE);
    }

    // v1g: validate the Pyth account's owner if PythConfig is passed.
    if let Some(cfg_ai) = pyth_config_ai {
        let expected_program = read_pyth_config_program_id(cfg_ai, program_id)?;
        if pyth_ai.owner() != &expected_program {
            return Err(err::PYTH_PROGRAM_ID_MISMATCH);
        }
    }

    // Parse the Pyth account first — it's the data we mutate the
    // feed with. If anything's wrong, we surface the failure before
    // touching the feed.
    let (raw_price, raw_conf, exponent) = read_pyth_price(pyth_ai)?;
    let conservative_price = apply_confidence_interval(raw_price, raw_conf)?;
    let price_micro_usd = normalize_to_micro_usd(conservative_price, exponent)?;

    let mut feed_data = feed_ai.try_borrow_mut_data()?;
    if feed_data.len() < PriceFeed::LEN {
        return Err(err::PRICE_FEED_PDA_MISMATCH);
    }
    let pf: &mut PriceFeed = bytemuck::try_from_bytes_mut(&mut feed_data[..PriceFeed::LEN])
        .map_err(|_| err::PRICE_FEED_LAYOUT_INVALID)?;
    let expected = create_program_address(&[seeds::PRICE_FEED, &pf.mint, &[pf.bump]], program_id)
        .map_err(|_| err::PRICE_FEED_PDA_MISMATCH)?;
    if &expected != feed_ai.key() {
        return Err(err::PRICE_FEED_PDA_MISMATCH);
    }
    if !pf.is_pyth_bound() {
        return Err(err::PRICE_FEED_NOT_PYTH_BOUND);
    }
    if &pf.pyth_source != pyth_ai.key() {
        return Err(err::PYTH_SOURCE_MISMATCH);
    }

    pf.price_micro_usd = price_micro_usd;
    pf.last_updated_slot = Clock::get()?.slot;

    msg!("ssr-compliance: update_price_from_pyth");
    Ok(())
}

/// Read `(price, conf, exponent)` from a Pyth `PriceUpdateV2`
/// account. Validates the discriminator + length before reading any
/// fields — substitution of a different Anchor account type rejects
/// with `PYTH_ACCOUNT_INVALID`.
fn read_pyth_price(pyth_ai: &AccountInfo) -> Result<(i64, u64, i32), ProgramError> {
    let data = pyth_ai.try_borrow_data()?;
    if data.len() < PYTH_PRICE_UPDATE_V2_LEN {
        return Err(err::PYTH_ACCOUNT_INVALID);
    }
    if data[..8] != PYTH_PRICE_UPDATE_V2_DISCRIMINATOR {
        return Err(err::PYTH_ACCOUNT_INVALID);
    }
    let price = i64::from_le_bytes(
        data[PYTH_OFFSET_PRICE..PYTH_OFFSET_PRICE + 8].try_into().unwrap(),
    );
    let conf = u64::from_le_bytes(
        data[PYTH_OFFSET_CONF..PYTH_OFFSET_CONF + 8].try_into().unwrap(),
    );
    let exponent = i32::from_le_bytes(
        data[PYTH_OFFSET_EXPONENT..PYTH_OFFSET_EXPONENT + 4]
            .try_into()
            .unwrap(),
    );
    Ok((price, conf, exponent))
}

/// Conservative pricing: subtract the confidence band from the
/// raw price. A non-positive result rejects with
/// `PYTH_NEGATIVE_PRICE` — clamping silently to 0 would let a
/// broken feed continue driving margin decisions.
fn apply_confidence_interval(price: i64, conf: u64) -> Result<u64, ProgramError> {
    if price <= 0 {
        return Err(err::PYTH_NEGATIVE_PRICE);
    }
    let p_u = price as u64;
    let conservative = p_u.checked_sub(conf).ok_or(err::PYTH_NEGATIVE_PRICE)?;
    if conservative == 0 {
        return Err(err::PYTH_NEGATIVE_PRICE);
    }
    Ok(conservative)
}

/// Convert Pyth's `(value, exponent)` representation into our
/// micro-USD scale (`actual_usd = value × 10^exponent`,
/// `micro_usd = actual_usd × 10^6 = value × 10^(exponent + 6)`).
///
/// For typical Pyth exponents (-9 ≤ exp ≤ -2) the factor is a
/// division by `10^|exp+6|`. We accept `exp + 6` in the range
/// `[-12, 6]` (covering essentially every Pyth feed in production)
/// and reject anything outside.
fn normalize_to_micro_usd(price: u64, exponent: i32) -> Result<u64, ProgramError> {
    let adjusted = exponent + 6;
    if !(-12..=6).contains(&adjusted) {
        return Err(err::PYTH_EXPONENT_OUT_OF_RANGE);
    }
    let value = if adjusted >= 0 {
        let factor = 10u128
            .checked_pow(adjusted as u32)
            .ok_or(err::PYTH_EXPONENT_OUT_OF_RANGE)?;
        (price as u128)
            .checked_mul(factor)
            .ok_or(err::PYTH_EXPONENT_OUT_OF_RANGE)?
    } else {
        let factor = 10u128
            .checked_pow((-adjusted) as u32)
            .ok_or(err::PYTH_EXPONENT_OUT_OF_RANGE)?;
        (price as u128) / factor
    };
    u64::try_from(value).map_err(|_| err::PYTH_EXPONENT_OUT_OF_RANGE)
}

// ─── initialize_pyth_config + set_pyth_program_id (Phase 4 v1g) ─────────-

/// Allocate the global `PythConfig` PDA and set its initial
/// `pyth_program_id`. Once allocated, subsequent
/// `bind_price_feed_to_pyth` / `update_price_from_pyth` calls
/// that pass this PDA become owner-validated.
///
/// Accounts:
///   [0, signer]         super_admin
///   [1, signer, write]  payer
///   [2, read]           registry PDA
///   [3, write]          pyth_config PDA (to be created)
///   [4, read]           system_program
///
/// Instruction data:
///   [0..32]   initial pyth_program_id
fn initialize_pyth_config(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (admin_ai, payer_ai, registry_ai, cfg_ai, _system_ai) = match accounts {
        [adm, pay, reg, cfg, sys, ..] => (adm, pay, reg, cfg, sys),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 32 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let mut pyth_program_id = [0u8; 32];
    pyth_program_id.copy_from_slice(&data[0..32]);

    require_signer(admin_ai)?;
    require_signer(payer_ai)?;
    let registry = read_registry(registry_ai, program_id)?;
    if registry.super_admin != *admin_ai.key() {
        return Err(err::UNAUTHORIZED_SUPER_ADMIN);
    }

    let (expected_pda, bump) = find_program_address(&[seeds::PYTH_CONFIG], program_id);
    if &expected_pda != cfg_ai.key() {
        return Err(err::PYTH_CONFIG_PDA_MISMATCH);
    }

    let lamports = Rent::get()?.minimum_balance(PythConfig::LEN);
    let bump_seed = [bump];
    let pda_seeds = [Seed::from(seeds::PYTH_CONFIG), Seed::from(&bump_seed[..])];
    let pda_signer = Signer::from(&pda_seeds);
    CreateAccount {
        from: payer_ai,
        to: cfg_ai,
        lamports,
        space: PythConfig::LEN as u64,
        owner: program_id,
    }
    .invoke_signed(&[pda_signer])?;

    let slot = Clock::get()?.slot;
    let mut cfg_data = cfg_ai.try_borrow_mut_data()?;
    let cfg: &mut PythConfig = bytemuck::try_from_bytes_mut(&mut cfg_data[..PythConfig::LEN])
        .map_err(|_| err::PYTH_CONFIG_LAYOUT_INVALID)?;
    *cfg = PythConfig::initial(pyth_program_id, slot, bump);

    msg!("ssr-compliance: initialize_pyth_config");
    Ok(())
}

/// Super-admin updates `PythConfig.pyth_program_id`.
///
/// Accounts:
///   [0, signer]   super_admin
///   [1, read]     registry PDA
///   [2, write]    pyth_config PDA
///
/// Instruction data:
///   [0..32]   new pyth_program_id
fn set_pyth_program_id(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (admin_ai, registry_ai, cfg_ai) = match accounts {
        [a, r, c, ..] => (a, r, c),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 32 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let mut new_id = [0u8; 32];
    new_id.copy_from_slice(&data[0..32]);

    require_signer(admin_ai)?;
    let registry = read_registry(registry_ai, program_id)?;
    if registry.super_admin != *admin_ai.key() {
        return Err(err::UNAUTHORIZED_SUPER_ADMIN);
    }

    let mut cfg_data = cfg_ai.try_borrow_mut_data()?;
    if cfg_data.len() < PythConfig::LEN {
        return Err(err::PYTH_CONFIG_PDA_MISMATCH);
    }
    let cfg: &mut PythConfig = bytemuck::try_from_bytes_mut(&mut cfg_data[..PythConfig::LEN])
        .map_err(|_| err::PYTH_CONFIG_LAYOUT_INVALID)?;
    let expected = create_program_address(&[seeds::PYTH_CONFIG, &[cfg.bump]], program_id)
        .map_err(|_| err::PYTH_CONFIG_PDA_MISMATCH)?;
    if &expected != cfg_ai.key() {
        return Err(err::PYTH_CONFIG_PDA_MISMATCH);
    }

    cfg.pyth_program_id = new_id;
    cfg.last_modified_slot = Clock::get()?.slot;

    msg!("ssr-compliance: set_pyth_program_id");
    Ok(())
}

// ─── initialize_extra_account_meta_list (Phase 0d) ────────────────────
//
// Token-2022 reads an `ExtraAccountMetaList` account (PDA: seeds =
// `[ "extra-account-metas", mint_pubkey ]` against this program) during
// a transfer to learn which additional accounts to forward to our hook
// (in our case: the source and destination `AccountRecord` PDAs).
//
// The account is in SPL TLV format — see `spl-tlv-account-resolution`.
// For a Phase 0c minimum we only set up the discriminator wiring; the
// TLV serializer + the (Literal "account_record", AccountKey(3)) seed
// configuration for each meta lands in Phase 0d. Issuers needing to test
// the hook end-to-end before then can pre-allocate the PDA by hand or
// use the SPL helper crate from off-chain code.

/// SPL `InitializeExtraAccountMetaList` instruction entrypoint.
///
/// Allocates the `ExtraAccountMetaList` PDA at
/// `seeds = [b"extra-account-metas", mint]` and writes the SSR meta
/// configuration: two PDA-derived extras, both `AccountRecord` PDAs
/// keyed by the owner pubkey of the source / destination token account
/// respectively (the owner is read via SPL `AccountData` seeds against
/// offset 32 of the token account, which is where SPL Token-2022 places
/// the owner field).
///
/// Accounts (per SPL hook-interface spec):
///   [0, signer, write]  payer (funds the new account)
///   [1, write]          ExtraAccountMetaList PDA (to be created)
///   [2, read]           mint
///   [3, signer]         mint authority (must match `mint.mint_authority`)
///   [4, read]           system_program
fn initialize_extra_account_meta_list(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
) -> ProgramResult {
    let (payer_ai, list_ai, mint_ai, mint_auth_ai, _system_program_ai) = match accounts {
        [p, list, m, ma, sys, ..] => (p, list, m, ma, sys),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    require_signer(payer_ai)?;
    require_signer(mint_auth_ai)?;
    enforce_mint_authority(mint_ai, mint_auth_ai)?;

    let mint_key = *mint_ai.key();
    let (expected_pda, bump) =
        find_program_address(&[seeds::EXTRA_META_LIST, &mint_key], program_id);
    if &expected_pda != list_ai.key() {
        return Err(err::META_LIST_PDA_MISMATCH);
    }

    let lamports = Rent::get()?.minimum_balance(extra_metas::ACCOUNT_SIZE);

    let bump_seed = [bump];
    let pda_seeds = [
        Seed::from(seeds::EXTRA_META_LIST),
        Seed::from(&mint_key[..]),
        Seed::from(&bump_seed[..]),
    ];
    let pda_signer = Signer::from(&pda_seeds);

    CreateAccount {
        from: payer_ai,
        to: list_ai,
        lamports,
        space: extra_metas::ACCOUNT_SIZE as u64,
        owner: program_id,
    }
    .invoke_signed(&[pda_signer])?;

    let mut data = list_ai.try_borrow_mut_data()?;
    extra_metas::write(&mut data[..extra_metas::ACCOUNT_SIZE])?;

    msg!("ssr-compliance: initialize_extra_account_meta_list");
    Ok(())
}

/// Verify that `mint_auth_ai.key()` matches the `mint_authority`
/// field in the SPL Token / Token-2022 mint account. The base mint
/// layout (which Token-2022 inherits) starts with
/// `mint_authority: COption<Pubkey>` at offset 0: a 4-byte LE
/// discriminator (`1` for `Some`, `0` for `None`), then the 32-byte
/// pubkey. Extensions follow the base layout but do not change the
/// offset of `mint_authority`.
fn enforce_mint_authority(mint_ai: &AccountInfo, mint_auth_ai: &AccountInfo) -> ProgramResult {
    let mint_data = mint_ai.try_borrow_data()?;
    if mint_data.len() < 36 {
        return Err(err::MINT_LAYOUT_INVALID);
    }
    let option_tag = u32::from_le_bytes(mint_data[0..4].try_into().unwrap());
    if option_tag != 1 {
        // Mint has no mint authority — issuance is frozen; rejecting
        // here is fail-closed (the meta list cannot be re-allocated
        // later by a different party if the authority is None).
        return Err(err::MINT_AUTHORITY_NONE);
    }
    let mint_authority: &[u8; 32] = mint_data[4..36].try_into().unwrap();
    if mint_authority != mint_auth_ai.key() {
        return Err(err::MINT_AUTHORITY_MISMATCH);
    }
    Ok(())
}

// ─── Shared helpers ──────────────────────────────────────────────────────

/// Reject if the account is not marked as a signer in the tx.
fn require_signer(ai: &AccountInfo) -> ProgramResult {
    if !ai.is_signer() {
        return Err(err::MISSING_SIGNER);
    }
    Ok(())
}

/// Borrow the registry and verify its PDA derivation against the cached bump.
/// Returns a borrowed copy (not a `&Registry`) so the caller can hold the
/// account borrow separately. Used by handlers that only need to *read*
/// the registry.
fn read_registry(ai: &AccountInfo, program_id: &Pubkey) -> Result<Registry, ProgramError> {
    let data = ai.try_borrow_data()?;
    if data.len() < Registry::LEN {
        return Err(err::REGISTRY_PDA_MISMATCH);
    }
    let r: &Registry = bytemuck::try_from_bytes(&data[..Registry::LEN])
        .map_err(|_| err::REGISTRY_PDA_MISMATCH)?;
    verify_registry_pda(ai, r, program_id)?;
    Ok(*r)
}

fn verify_registry_pda(
    ai: &AccountInfo,
    r: &Registry,
    program_id: &Pubkey,
) -> ProgramResult {
    let expected = create_program_address(&[seeds::REGISTRY, &[r.bump]], program_id)
        .map_err(|_| err::REGISTRY_PDA_MISMATCH)?;
    if &expected != ai.key() {
        return Err(err::REGISTRY_PDA_MISMATCH);
    }
    Ok(())
}

fn verify_record_pda(
    ai: &AccountInfo,
    r: &AccountRecord,
    program_id: &Pubkey,
) -> ProgramResult {
    let expected = create_program_address(
        &[seeds::ACCOUNT_RECORD, &r.participant, &[r.bump]],
        program_id,
    )
    .map_err(|_| err::RECORD_PDA_MISMATCH)?;
    if &expected != ai.key() {
        return Err(err::RECORD_PDA_MISMATCH);
    }
    Ok(())
}

// ─── Read-side: transfer-hook gate ───────────────────────────────────────

/// Token-2022 transfer-hook entrypoint.
///
/// Account ordering follows the SPL Transfer Hook interface, where the
/// validation account (the `ExtraAccountMetaList` PDA) is appended after
/// the four required accounts, and the two SSR-declared extras follow:
///
/// | Index | Account                                           | Notes |
/// |-------|---------------------------------------------------|-------|
/// | 0     | Source token account                              | per SPL spec |
/// | 1     | Mint                                              | per SPL spec |
/// | 2     | Destination token account                         | per SPL spec |
/// | 3     | Source authority                                  | per SPL spec |
/// | 4     | `ExtraAccountMetaList` PDA                        | per SPL spec |
/// | 5     | Source owner's `AccountRecord` PDA                | SSR meta 0 |
/// | 6     | Destination owner's `AccountRecord` PDA           | SSR meta 1 |
///
/// Both legs must be `VERIFIED`. Unknown status discriminants reject
/// rather than silently permitting. The validation PDA at index 4 is
/// passed through but not re-verified here — Token-2022 already
/// resolved the extras against it before the CPI.
fn check_transfer(accounts: &[AccountInfo]) -> ProgramResult {
    if accounts.len() < 7 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    enforce_verified(&accounts[5])?;
    enforce_verified(&accounts[6])?;
    Ok(())
}

/// Wrap an `AccountInfo` borrow around the pure-bytes check. Kept as a
/// thin shell so `check_record_bytes` can be exercised directly from
/// host-side unit and property tests without mocking `AccountInfo`.
fn enforce_verified(record_account: &AccountInfo) -> ProgramResult {
    let data = record_account.try_borrow_data()?;
    check_record_bytes(&data)
}

/// Decide whether a raw account-data buffer represents an
/// `AccountRecord` whose participant is currently cleared to transfer.
///
/// Thin shim over `ssr_types::check_record_bytes`: the decision logic
/// lives in the shared crate so composition wrappers (DvP, margin,
/// repo, ...) can call the same check without depending on this
/// program's error namespace. Here we just translate `CheckError` into
/// `ssr-compliance`'s own `ProgramError::Custom` codes so transfer-hook
/// log lines stay in the 0x1001-0x102B range operators already know.
///
/// Failure modes are deliberately distinct so transfer hooks can log a
/// specific error code; operators reviewing rejected transfers need to
/// tell "suspended (temporary)" from "blocked (permanent)" from
/// "unverified (never onboarded)" from "corrupt record" without parsing
/// the buffer themselves.
pub fn check_record_bytes(bytes: &[u8]) -> ProgramResult {
    ssr_types::check_record_bytes(bytes).map_err(check_error_to_program_error)
}

/// Translate the shared `CheckError` into this program's local error
/// namespace. Exposed in case integrators want to reuse the mapping.
#[must_use]
pub fn check_error_to_program_error(e: CheckError) -> ProgramError {
    match e {
        CheckError::LayoutInvalid => err::RECORD_LAYOUT_INVALID,
        CheckError::StatusUnknown => err::RECORD_STATUS_UNKNOWN,
        CheckError::Unverified => err::TRANSFER_DENIED_UNVERIFIED,
        CheckError::Suspended => err::TRANSFER_DENIED_SUSPENDED,
        CheckError::Blocked => err::TRANSFER_DENIED_BLOCKED,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::bytes_of;
    use proptest::prelude::*;
    use ssr_types::{flags, jurisdiction};

    /// Build an `AccountRecord` whose only meaningful field is `status`.
    /// Used by every test to isolate the status-decision logic.
    fn record_with_status(status: u8) -> AccountRecord {
        AccountRecord {
            updated_at_slot: 100,
            participant: [1; 32],
            status,
            jurisdiction: jurisdiction::JP,
            flags: 0,
            bump: 0,
            _reserved: [0; 11],
        }
    }

    fn check_status(status: u8) -> ProgramResult {
        let r = record_with_status(status);
        check_record_bytes(bytes_of(&r))
    }

    // ── Per-discriminant outcomes ──

    #[test]
    fn verified_status_passes() {
        assert!(check_status(compliance_status::VERIFIED).is_ok());
    }

    #[test]
    fn suspended_status_returns_suspended_error() {
        assert_eq!(
            check_status(compliance_status::SUSPENDED).unwrap_err(),
            err::TRANSFER_DENIED_SUSPENDED
        );
    }

    #[test]
    fn blocked_status_returns_blocked_error() {
        assert_eq!(
            check_status(compliance_status::BLOCKED).unwrap_err(),
            err::TRANSFER_DENIED_BLOCKED
        );
    }

    #[test]
    fn unknown_known_status_returns_unverified_error() {
        // `UNKNOWN` is a recognized discriminant (= 0) but is not VERIFIED,
        // so the gate rejects with the UNVERIFIED variant — distinct from
        // an out-of-range status byte.
        assert_eq!(
            check_status(compliance_status::UNKNOWN).unwrap_err(),
            err::TRANSFER_DENIED_UNVERIFIED
        );
    }

    #[test]
    fn pending_status_returns_unverified_error() {
        assert_eq!(
            check_status(compliance_status::PENDING).unwrap_err(),
            err::TRANSFER_DENIED_UNVERIFIED
        );
    }

    #[test]
    fn out_of_range_status_returns_unknown_record_error() {
        // 99 is neither UNKNOWN..=BLOCKED nor a known reserved value.
        // Programs must fail closed: don't silently treat unknowns as
        // "unverified", flag them as corrupt-record so operators see
        // it as a separate incident class.
        assert_eq!(
            check_status(99).unwrap_err(),
            err::RECORD_STATUS_UNKNOWN
        );
    }

    // ── Buffer-shape outcomes ──

    #[test]
    fn empty_buffer_returns_layout_invalid() {
        assert_eq!(
            check_record_bytes(&[]).unwrap_err(),
            err::RECORD_LAYOUT_INVALID
        );
    }

    #[test]
    fn short_buffer_returns_layout_invalid() {
        let buf = [0u8; AccountRecord::LEN - 1];
        assert_eq!(
            check_record_bytes(&buf).unwrap_err(),
            err::RECORD_LAYOUT_INVALID
        );
    }

    #[test]
    fn exact_len_buffer_decodes() {
        let r = record_with_status(compliance_status::VERIFIED);
        let buf = bytes_of(&r);
        assert_eq!(buf.len(), AccountRecord::LEN);
        assert!(check_record_bytes(buf).is_ok());
    }

    #[test]
    fn extra_trailing_bytes_are_ignored() {
        // Programs allocate accounts at `AccountRecord::LEN`, but if a
        // caller passes a longer buffer (e.g., a re-used account with
        // extra space), the gate must read only the header and not
        // reject. This protects against forced denial-of-service via
        // oversized buffers.
        let r = record_with_status(compliance_status::VERIFIED);
        let mut buf = [0u8; AccountRecord::LEN + 64];
        buf[..AccountRecord::LEN].copy_from_slice(bytes_of(&r));
        assert!(check_record_bytes(&buf).is_ok());
    }

    #[test]
    fn flags_do_not_affect_gate_decision() {
        // Accredited / professional / regulated flags are read by
        // downstream programs (e.g., asset-class issuance gates) but
        // must NOT affect the basic transfer-permission check.
        let mut r = record_with_status(compliance_status::VERIFIED);
        r.flags = flags::ACCREDITED | flags::PROFESSIONAL | flags::REGULATED_ENTITY;
        assert!(check_record_bytes(bytes_of(&r)).is_ok());
    }

    // ── Property tests ──

    proptest! {
        /// For any `status` byte, the gate must return a deterministic,
        /// well-defined outcome — never panic, never silently pass an
        /// unknown discriminant.
        #[test]
        fn status_byte_never_panics(status in any::<u8>()) {
            let r = record_with_status(status);
            let outcome = check_record_bytes(bytes_of(&r));
            // Outcome is one of: Ok, or one of the 4 deny-reasons, or
            // record-status-unknown. Never anything else.
            match outcome {
                Ok(()) => prop_assert_eq!(status, compliance_status::VERIFIED),
                Err(e) if e == err::TRANSFER_DENIED_SUSPENDED =>
                    prop_assert_eq!(status, compliance_status::SUSPENDED),
                Err(e) if e == err::TRANSFER_DENIED_BLOCKED =>
                    prop_assert_eq!(status, compliance_status::BLOCKED),
                Err(e) if e == err::TRANSFER_DENIED_UNVERIFIED => {
                    prop_assert!(status == compliance_status::UNKNOWN
                        || status == compliance_status::PENDING);
                }
                Err(e) if e == err::RECORD_STATUS_UNKNOWN => {
                    prop_assert!(!compliance_status::is_known(status));
                }
                Err(e) => prop_assert!(false, "unexpected error variant: {e:?}"),
            }
        }

        /// Arbitrary buffer lengths must not panic. Short buffers reject
        /// with layout-invalid; exact / longer buffers decode the header.
        #[test]
        fn arbitrary_buffer_length_never_panics(len in 0usize..512) {
            let buf = vec![0u8; len];
            let outcome = check_record_bytes(&buf);
            if len < AccountRecord::LEN {
                prop_assert_eq!(outcome.unwrap_err(), err::RECORD_LAYOUT_INVALID);
            } else {
                // Zeroed buffer => status == UNKNOWN => unverified.
                prop_assert_eq!(outcome.unwrap_err(), err::TRANSFER_DENIED_UNVERIFIED);
            }
        }

        /// Fuzz the entire 56-byte buffer. The gate must never panic and
        /// must always classify into one of the five outcomes.
        #[test]
        fn random_record_bytes_never_panic(buf in proptest::array::uniform32(any::<u8>())
            .prop_flat_map(|h32| (Just(h32), proptest::array::uniform24(any::<u8>()))))
        {
            let (head, tail) = buf;
            let mut full = [0u8; AccountRecord::LEN];
            full[..32].copy_from_slice(&head);
            full[32..56].copy_from_slice(&tail);
            let _ = check_record_bytes(&full);
            // The assertion is "doesn't panic"; the call above provides it.
        }
    }

    // ── SPL discriminator verification ──

    /// Both 8-byte SPL discriminators are derived as
    /// `sha256("spl-transfer-hook-interface:<method>")[..8]`. Recomputing
    /// them here catches the case where the SPL namespace or method
    /// strings change upstream — far better to fail this test than to
    /// ship a hook program that silently ignores Token-2022 transfers.
    #[test]
    fn spl_discriminators_match_sha256() {
        use sha2::{Digest, Sha256};

        fn derive(name: &str) -> [u8; 8] {
            let h = Sha256::digest(name.as_bytes());
            let mut out = [0u8; 8];
            out.copy_from_slice(&h[..8]);
            out
        }

        assert_eq!(
            hook_disc::EXECUTE,
            derive("spl-transfer-hook-interface:execute"),
            "hook_disc::EXECUTE drift — SPL spec changed?"
        );
        assert_eq!(
            hook_disc::INITIALIZE_EXTRA_ACCOUNT_METAS,
            derive("spl-transfer-hook-interface:initialize-extra-account-metas"),
            "hook_disc::INITIALIZE_EXTRA_ACCOUNT_METAS drift — SPL spec changed?"
        );
    }

    /// The two SPL discriminators must not collide with each other and
    /// must not collide with the first byte of any internal admin tag
    /// (otherwise a single-byte admin call could be misrouted as a hook
    /// call once the dispatch detects an 8-byte prefix match by accident).
    #[test]
    fn spl_discriminators_do_not_collide_with_admin_tags() {
        assert_ne!(hook_disc::EXECUTE, hook_disc::INITIALIZE_EXTRA_ACCOUNT_METAS);
        for tag in [
            ix::INITIALIZE_REGISTRY,
            ix::REGISTER_ACCOUNT,
            ix::UPDATE_STATUS,
            ix::CHECK_TRANSFER,
            ix::ROTATE_OPERATORS,
        ] {
            assert_ne!(tag, hook_disc::EXECUTE[0]);
            assert_ne!(tag, hook_disc::INITIALIZE_EXTRA_ACCOUNT_METAS[0]);
        }
    }

    // ── ExtraAccountMetaList byte layout: golden bytes vs SPL reference ──

    #[test]
    fn extra_metas_size_matches_spl_reference() {
        use spl_tlv_account_resolution::state::ExtraAccountMetaList;

        assert_eq!(
            extra_metas::ACCOUNT_SIZE,
            ExtraAccountMetaList::size_of(extra_metas::NUM_METAS).unwrap(),
            "extra_metas::ACCOUNT_SIZE drifted from SPL ExtraAccountMetaList::size_of"
        );
    }

    /// Hand-rolled `extra_metas::write` must produce a buffer that the
    /// SPL reference parser accepts AND whose `ExtraAccountMeta` entries
    /// match the ones we'd construct via the high-level SPL API. This
    /// catches any drift in: TLV header bytes, PodSlice header layout,
    /// per-entry discriminator / address_config / signer / writable
    /// encoding, and seed packing.
    #[test]
    fn extra_metas_layout_matches_spl_reference() {
        use spl_tlv_account_resolution::{
            account::ExtraAccountMeta as RefExtraAccountMeta, seeds::Seed as RefSeed,
            state::ExtraAccountMetaList,
        };
        use spl_transfer_hook_interface::instruction::ExecuteInstruction;

        // Build the same two metas via the SPL high-level API.
        let source_meta = RefExtraAccountMeta::new_with_seeds(
            &[
                RefSeed::Literal {
                    bytes: ssr_types::seeds::ACCOUNT_RECORD.to_vec(),
                },
                RefSeed::AccountData {
                    account_index: 0,
                    data_index: 32,
                    length: 32,
                },
            ],
            false,
            false,
        )
        .unwrap();
        let dest_meta = RefExtraAccountMeta::new_with_seeds(
            &[
                RefSeed::Literal {
                    bytes: ssr_types::seeds::ACCOUNT_RECORD.to_vec(),
                },
                RefSeed::AccountData {
                    account_index: 2,
                    data_index: 32,
                    length: 32,
                },
            ],
            false,
            false,
        )
        .unwrap();

        // Build the reference byte buffer via the SPL ExtraAccountMetaList::init API.
        let mut spl_bytes = vec![0u8; extra_metas::ACCOUNT_SIZE];
        ExtraAccountMetaList::init::<ExecuteInstruction>(&mut spl_bytes, &[source_meta, dest_meta])
            .unwrap();

        // Build ours.
        let mut ours = vec![0u8; extra_metas::ACCOUNT_SIZE];
        extra_metas::write(&mut ours).unwrap();

        assert_eq!(ours, spl_bytes, "extra_metas::write drifted from SPL layout");
    }

    #[test]
    fn extra_metas_write_rejects_wrong_buffer_size() {
        let mut too_short = vec![0u8; extra_metas::ACCOUNT_SIZE - 1];
        assert!(extra_metas::write(&mut too_short).is_err());

        let mut too_long = vec![0u8; extra_metas::ACCOUNT_SIZE + 1];
        assert!(extra_metas::write(&mut too_long).is_err());
    }
}
