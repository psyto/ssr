//! `ssr-dvp-wrapper` — compliance-gated atomic DvP settlement.
//!
//! Thin Pinocchio program that sits in front of the SPC DvP Swap
//! Program (`DzG1qJupt6Khm8s8jB3p93NkhPoiAg2M7vkEhkS15CtC`) as the
//! `settlement_authority`. SPC handles the atomic 2-leg swap mechanics,
//! TransferHook extras forwarding, surplus refunds, expiry, etc.
//! This wrapper adds exactly one thing: it verifies both parties'
//! `AccountRecord`s pass `ssr_types::check_record_bytes` before signing
//! the `SettleDvp` CPI.
//!
//! The wrapper has a single PDA `seeds = [b"dvp_authority"]` per
//! deployment. Anyone can create a `SwapDvp` on SPC with this PDA as the
//! `settlement_authority`; only this program (signing as its PDA) can
//! settle it, and only after both parties verify.

#![cfg_attr(not(test), no_std)]

use pinocchio::{
    ProgramResult, account_info::AccountInfo,
    cpi::slice_invoke_signed,
    instruction::{AccountMeta, Instruction, Seed, Signer},
    msg,
    program_error::ProgramError,
    pubkey::{Pubkey, find_program_address},
};
use ssr_types::CheckError;

// Program scaffolding. See `ssr-compliance/src/lib.rs` for why the
// `entrypoint!` macro is split.
pinocchio::program_entrypoint!(process_instruction);
pinocchio::default_allocator!();
pinocchio::nostd_panic_handler!();

// ─── Constants ───────────────────────────────────────────────────────────

/// Seed for the wrapper's settlement-authority PDA. Deterministic per
/// deployment of this program.
pub const AUTHORITY_SEED: &[u8] = b"dvp_authority";

/// SPC `dvp-swap-program` ID. Decoded at compile time from the base58
/// the SPC repo publishes (`DzG1qJupt6Khm8s8jB3p93NkhPoiAg2M7vkEhkS15CtC`)
/// so we don't hand-copy 32 bytes that drift silently from the source.
pub const SPC_DVP_PROGRAM_ID: Pubkey =
    pinocchio_pubkey::from_str("DzG1qJupt6Khm8s8jB3p93NkhPoiAg2M7vkEhkS15CtC");

/// `dvp-swap-program` discriminator for `SettleDvp`.
pub const SPC_IX_SETTLE_DVP: u8 = 2;

// ─── Instruction discriminators ──────────────────────────────────────────

/// Wrapper instructions. Disjoint from SPC's namespace.
pub mod ix {
    /// Verify both parties' compliance, then CPI into
    /// `dvp-swap-program::SettleDvp`.
    pub const COMPLIANT_SETTLE_DVP: u8 = 0;
}

// ─── Custom error codes ──────────────────────────────────────────────────
//
// 0x2000-0x2FFF reserved for ssr-dvp-wrapper. Disjoint from
// ssr-compliance's 0x1000-0x1FFF.

pub mod err {
    use pinocchio::program_error::ProgramError;

    // Account-shape failures.
    pub const RECORD_OWNER_MISMATCH: ProgramError = ProgramError::Custom(0x2001);
    pub const RECORD_PARTICIPANT_MISMATCH: ProgramError = ProgramError::Custom(0x2002);
    pub const SWAP_DVP_OWNER_MISMATCH: ProgramError = ProgramError::Custom(0x2003);
    pub const SETTLEMENT_AUTHORITY_MISMATCH: ProgramError = ProgramError::Custom(0x2004);
    pub const SWAP_DVP_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x2005);
    pub const SPC_PROGRAM_ID_MISMATCH: ProgramError = ProgramError::Custom(0x2006);
    pub const INSTRUCTION_DATA_TOO_SHORT: ProgramError = ProgramError::Custom(0x2007);

    // Compliance gate failures, mirrored from `ssr_types::CheckError`.
    // Disjoint from ssr-compliance's so a wrapper-side reject is
    // distinguishable in logs from a hook-side reject.
    pub const COMPLIANCE_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x2010);
    pub const COMPLIANCE_STATUS_UNKNOWN: ProgramError = ProgramError::Custom(0x2011);
    pub const COMPLIANCE_UNVERIFIED: ProgramError = ProgramError::Custom(0x2012);
    pub const COMPLIANCE_SUSPENDED: ProgramError = ProgramError::Custom(0x2013);
    pub const COMPLIANCE_BLOCKED: ProgramError = ProgramError::Custom(0x2014);
}

// ─── Entrypoint dispatch ─────────────────────────────────────────────────

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (tag, rest) = data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;
    match *tag {
        ix::COMPLIANT_SETTLE_DVP => compliant_settle_dvp(program_id, accounts, rest),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

// ─── SwapDvp layout offsets (mirrors SPC `state::swap_dvp::SwapDvp`) ─────
//
//   offset  size  field
//   0       1     bump
//   1       32    user_a
//   33      32    user_b
//   65      32    mint_a
//   97      32    mint_b
//   129     32    settlement_authority
//   161     8     amount_a
//   ...
//
// Layout pinned in SPC's `swap_dvp::SwapDvp::to_bytes`. If they ever
// reorder, this wrapper rejects with `SWAP_DVP_LAYOUT_INVALID` because
// the settlement-authority pubkey at offset 129 will no longer match.

const OFFSET_USER_A: usize = 1;
const OFFSET_USER_B: usize = 33;
const OFFSET_SETTLEMENT_AUTHORITY: usize = 129;
/// Minimum bytes we need to inspect.
const MIN_SWAP_DVP_LEN: usize = OFFSET_SETTLEMENT_AUTHORITY + 32; // = 161

/// Parsed accessor for the only `SwapDvp` fields we need to read on
/// settlement. Borrows the underlying byte buffer; no allocation.
struct SwapDvpView<'a> {
    user_a: &'a [u8; 32],
    user_b: &'a [u8; 32],
    settlement_authority: &'a [u8; 32],
}

impl<'a> SwapDvpView<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, ProgramError> {
        if bytes.len() < MIN_SWAP_DVP_LEN {
            return Err(err::SWAP_DVP_LAYOUT_INVALID);
        }
        Ok(Self {
            user_a: bytes[OFFSET_USER_A..OFFSET_USER_A + 32].try_into().unwrap(),
            user_b: bytes[OFFSET_USER_B..OFFSET_USER_B + 32].try_into().unwrap(),
            settlement_authority: bytes[OFFSET_SETTLEMENT_AUTHORITY
                ..OFFSET_SETTLEMENT_AUTHORITY + 32]
                .try_into()
                .unwrap(),
        })
    }
}

// ─── compliant_settle_dvp ────────────────────────────────────────────────
//
// Account layout (writable/signer flags reflect what we forward to SPC):
//
//   [0,  signer (PDA), write]  wrapper_authority_pda (becomes SPC SettleDvp accounts[0])
//   [1,  read]                 ssr_compliance_program_id (owner reference for record accounts)
//   [2,  read]                 spc_dvp_program (CPI target, executable)
//   [3,  read]                 user_a's AccountRecord PDA
//   [4,  read]                 user_b's AccountRecord PDA
//   [5,  write]                swap_dvp (SPC PDA)                  → SPC[1]
//   [6,  read]                 mint_a                              → SPC[2]
//   [7,  read]                 mint_b                              → SPC[3]
//   [8,  write]                dvp_ata_a (escrow A)                → SPC[4]
//   [9,  write]                dvp_ata_b (escrow B)                → SPC[5]
//   [10, write]                user_a_ata_b                        → SPC[6]
//   [11, write]                user_b_ata_a                        → SPC[7]
//   [12, write]                user_a_ata_a (surplus refund)       → SPC[8]
//   [13, write]                user_b_ata_b (surplus refund)       → SPC[9]
//   [14, read]                 token_program_a                     → SPC[10]
//   [15, read]                 token_program_b                     → SPC[11]
//   [16..N]                    leg_a / leg_b TransferHook extras, forwarded as-is
//
// Instruction data (after the dispatch tag): single byte
//   [0]  leg_a_extras_count — split point between leg-A and leg-B
//        trailing accounts; forwarded verbatim to SPC SettleDvp.

const FIXED_ACCOUNTS_LEN: usize = 16;

fn compliant_settle_dvp(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if accounts.len() < FIXED_ACCOUNTS_LEN {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    if data.is_empty() {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let leg_a_extras_count = data[0] as usize;

    let wrapper_authority_ai = &accounts[0];
    let ssr_compliance_program_ai = &accounts[1];
    let spc_dvp_program_ai = &accounts[2];
    let user_a_record_ai = &accounts[3];
    let user_b_record_ai = &accounts[4];
    let swap_dvp_ai = &accounts[5];
    let forwarded = &accounts[5..]; // [5..16] = SPC fixed accounts[1..12]; [16..] = extras

    if spc_dvp_program_ai.key() != &SPC_DVP_PROGRAM_ID {
        return Err(err::SPC_PROGRAM_ID_MISMATCH);
    }

    // 1. Derive + verify the wrapper's settlement-authority PDA. We do
    //    this against the *current* program_id so a redeploy with a
    //    different program ID gets a different authority and cannot
    //    accidentally settle DvPs created against the old deployment.
    let (expected_authority, authority_bump) =
        find_program_address(&[AUTHORITY_SEED], program_id);
    if wrapper_authority_ai.key() != &expected_authority {
        return Err(err::SETTLEMENT_AUTHORITY_MISMATCH);
    }

    // 2. Parse the SwapDvp account and confirm it really designates our
    //    PDA as settlement_authority (otherwise we'd be signing for
    //    a DvP we don't actually own).
    if swap_dvp_ai.owner() != &SPC_DVP_PROGRAM_ID {
        return Err(err::SWAP_DVP_OWNER_MISMATCH);
    }
    let swap_dvp_data = swap_dvp_ai.try_borrow_data()?;
    let swap = SwapDvpView::parse(&swap_dvp_data)?;
    if swap.settlement_authority != &expected_authority {
        return Err(err::SETTLEMENT_AUTHORITY_MISMATCH);
    }

    let user_a = *swap.user_a;
    let user_b = *swap.user_b;
    drop(swap_dvp_data); // release borrow before CPI

    // 3. Verify both AccountRecord PDAs.
    verify_record(
        user_a_record_ai,
        ssr_compliance_program_ai.key(),
        &user_a,
    )?;
    verify_record(
        user_b_record_ai,
        ssr_compliance_program_ai.key(),
        &user_b,
    )?;

    // 4. CPI into SPC SettleDvp signing as our PDA. Account list mirrors
    //    SPC's expectation; trailing extras (transfer-hook accounts,
    //    if any) ride through unchanged.
    cpi_settle_dvp(
        wrapper_authority_ai,
        forwarded,
        authority_bump,
        leg_a_extras_count,
    )?;

    msg!("ssr-dvp-wrapper: compliant_settle_dvp");
    Ok(())
}

/// Compliance + binding check on one `AccountRecord` PDA.
///
/// The wrapper does not re-derive the record PDA from
/// `seeds::ACCOUNT_RECORD ++ participant` — that's an extra ~1500 CU
/// per leg. The owner check + the `record.participant` field check
/// suffice because `ssr-compliance` only writes records at the
/// canonical PDA, so a record whose `participant` is X and whose owner
/// is `ssr-compliance` must be the canonical record for X.
fn verify_record(
    record_ai: &AccountInfo,
    compliance_program: &Pubkey,
    expected_participant: &Pubkey,
) -> ProgramResult {
    if record_ai.owner() != compliance_program {
        return Err(err::RECORD_OWNER_MISMATCH);
    }
    let data = record_ai.try_borrow_data()?;
    let record = ssr_types::read_account_record(&data).map_err(check_error_to_program_error)?;
    if &record.participant != expected_participant {
        return Err(err::RECORD_PARTICIPANT_MISMATCH);
    }
    record
        .check_transfer_allowed()
        .map_err(check_error_to_program_error)
}

fn check_error_to_program_error(e: CheckError) -> ProgramError {
    match e {
        CheckError::LayoutInvalid => err::COMPLIANCE_LAYOUT_INVALID,
        CheckError::StatusUnknown => err::COMPLIANCE_STATUS_UNKNOWN,
        CheckError::Unverified => err::COMPLIANCE_UNVERIFIED,
        CheckError::Suspended => err::COMPLIANCE_SUSPENDED,
        CheckError::Blocked => err::COMPLIANCE_BLOCKED,
    }
}

// ─── CPI into SPC SettleDvp ──────────────────────────────────────────────
//
// `forwarded[0]` = swap_dvp (== SPC SettleDvp accounts[1])
// `forwarded[1..11]` = SPC SettleDvp accounts[2..12]
// `forwarded[11..]` = leg_a + leg_b trailing extras
//
// SPC SettleDvp expects accounts[0] = settlement_authority, which is
// our PDA. We push it ahead of the forwarded slice.

fn cpi_settle_dvp(
    wrapper_authority_ai: &AccountInfo,
    forwarded: &[AccountInfo],
    authority_bump: u8,
    leg_a_extras_count: usize,
) -> ProgramResult {
    // forwarded.len() must be at least the 11 SPC fixed accounts after
    // settlement_authority. trailing extras may be 0 or more.
    if forwarded.len() < 11 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }

    let total_accounts = 1 + forwarded.len();
    let mut metas: alloc::vec::Vec<AccountMeta> = alloc::vec::Vec::with_capacity(total_accounts);
    let mut infos: alloc::vec::Vec<&AccountInfo> =
        alloc::vec::Vec::with_capacity(total_accounts);

    // settlement_authority = our PDA (signer, writable per SPC spec)
    metas.push(AccountMeta::new(wrapper_authority_ai.key(), true, true));
    infos.push(wrapper_authority_ai);

    // swap_dvp = forwarded[0] (writable)
    metas.push(AccountMeta::new(forwarded[0].key(), true, false));
    infos.push(&forwarded[0]);

    // mint_a, mint_b = forwarded[1..3] (readonly)
    metas.push(AccountMeta::new(forwarded[1].key(), false, false));
    infos.push(&forwarded[1]);
    metas.push(AccountMeta::new(forwarded[2].key(), false, false));
    infos.push(&forwarded[2]);

    // 6 writable ATAs = forwarded[3..9]
    for i in 3..9 {
        metas.push(AccountMeta::new(forwarded[i].key(), true, false));
        infos.push(&forwarded[i]);
    }

    // 2 token programs = forwarded[9..11] (readonly)
    metas.push(AccountMeta::new(forwarded[9].key(), false, false));
    infos.push(&forwarded[9]);
    metas.push(AccountMeta::new(forwarded[10].key(), false, false));
    infos.push(&forwarded[10]);

    // Trailing extras: leg_a then leg_b. Mark all as readonly — the
    // SPL TransferHook ABI never escalates an extra to signer/writable
    // here (extras' privilege is determined by their `is_signer` /
    // `is_writable` fields inside the `ExtraAccountMetaList`, which SPC
    // applies on its side).
    let extras = &forwarded[11..];
    if leg_a_extras_count > extras.len() {
        return Err(ProgramError::InvalidInstructionData);
    }
    for ai in extras {
        metas.push(AccountMeta::new(ai.key(), false, false));
        infos.push(ai);
    }

    let ix = Instruction {
        program_id: &SPC_DVP_PROGRAM_ID,
        data: &[SPC_IX_SETTLE_DVP, leg_a_extras_count as u8],
        accounts: &metas,
    };

    let bump_seed = [authority_bump];
    let pda_seeds = [Seed::from(AUTHORITY_SEED), Seed::from(&bump_seed[..])];
    let pda_signer = Signer::from(&pda_seeds);

    slice_invoke_signed(&ix, &infos, &[pda_signer])
}

extern crate alloc;

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn build_swap_dvp(
        user_a: [u8; 32],
        user_b: [u8; 32],
        settlement_authority: [u8; 32],
    ) -> [u8; MIN_SWAP_DVP_LEN] {
        let mut buf = [0u8; MIN_SWAP_DVP_LEN];
        buf[OFFSET_USER_A..OFFSET_USER_A + 32].copy_from_slice(&user_a);
        buf[OFFSET_USER_B..OFFSET_USER_B + 32].copy_from_slice(&user_b);
        buf[OFFSET_SETTLEMENT_AUTHORITY..OFFSET_SETTLEMENT_AUTHORITY + 32]
            .copy_from_slice(&settlement_authority);
        buf
    }

    #[test]
    fn swap_dvp_view_parses_three_fields() {
        let buf = build_swap_dvp([1; 32], [2; 32], [3; 32]);
        let view = SwapDvpView::parse(&buf).unwrap();
        assert_eq!(view.user_a, &[1u8; 32]);
        assert_eq!(view.user_b, &[2u8; 32]);
        assert_eq!(view.settlement_authority, &[3u8; 32]);
    }

    #[test]
    fn swap_dvp_view_rejects_short_buffer() {
        let short = vec![0u8; MIN_SWAP_DVP_LEN - 1];
        assert_eq!(
            SwapDvpView::parse(&short).err(),
            Some(err::SWAP_DVP_LAYOUT_INVALID)
        );
    }

    #[test]
    fn swap_dvp_view_accepts_oversized_buffer() {
        // SPC `SwapDvp::LEN` (210 bytes) is larger than `MIN_SWAP_DVP_LEN`
        // (161). We must not reject just because the account holds the
        // amounts / nonce / earliest_settlement that we don't read.
        let big = vec![0u8; 256];
        // Fill in the three fields we do read.
        let mut buf = big;
        buf[OFFSET_USER_A] = 1;
        buf[OFFSET_USER_B] = 2;
        buf[OFFSET_SETTLEMENT_AUTHORITY] = 3;
        let view = SwapDvpView::parse(&buf).unwrap();
        assert_eq!(view.user_a[0], 1);
        assert_eq!(view.user_b[0], 2);
        assert_eq!(view.settlement_authority[0], 3);
    }

    #[test]
    fn check_error_mapping_is_distinct_per_variant() {
        // Each variant maps to a distinct ProgramError::Custom code.
        let mapped = [
            check_error_to_program_error(CheckError::LayoutInvalid),
            check_error_to_program_error(CheckError::StatusUnknown),
            check_error_to_program_error(CheckError::Unverified),
            check_error_to_program_error(CheckError::Suspended),
            check_error_to_program_error(CheckError::Blocked),
        ];
        // Sanity: each is distinct.
        for i in 0..mapped.len() {
            for j in (i + 1)..mapped.len() {
                assert_ne!(mapped[i], mapped[j], "{i} and {j} collide");
            }
        }
        // And they live in the 0x20XX namespace, not 0x10XX
        // (ssr-compliance's), so logs disambiguate.
        for e in mapped {
            if let ProgramError::Custom(code) = e {
                assert!(
                    code >= 0x2010 && code < 0x2020,
                    "code 0x{code:04X} outside wrapper compliance band"
                );
            } else {
                panic!("expected Custom variant");
            }
        }
    }
}
