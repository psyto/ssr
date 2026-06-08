//! `ssr-repo` — compliance-gated bilateral time-bound lock.
//!
//! Minimum Phase 3 surface: `open_repo` locks both parties' positions
//! (collateral on the borrower side, cash on the lender side) against
//! the `Repo` PDA as the `lock_authority`; `close_repo` releases both
//! locks when the borrower has fulfilled the obligation (typically by
//! returning the cash leg via a separate transfer the wrapper does
//! not handle in Phase 3 minimum). The post-expiry default path —
//! lender takes the locked collateral when the borrower fails to
//! repay — is reserved for Phase 3b.
//!
//! The program does not move tokens; it only flips lock state in
//! `ssr-vault` via CPI. Cash flows between parties are the client's
//! responsibility for Phase 3 minimum, mirroring how real-world repo
//! desks handle the leg movements separately from the encumbrance.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use pinocchio::{
    ProgramResult, account_info::AccountInfo,
    instruction::{AccountMeta, Instruction, Seed, Signer},
    msg,
    program_error::ProgramError,
    pubkey::{Pubkey, create_program_address, find_program_address},
    sysvars::{Sysvar, clock::Clock, rent::Rent},
};
use pinocchio_system::instructions::CreateAccount;
use ssr_types::{CheckError, Repo, repo_status, seeds};

// Mirror of `ssr_vault::ix::LOCK_POSITION` / `UNLOCK_POSITION`. See the
// note in `Cargo.toml` for why we duplicate rather than depend; if
// `ssr-vault` ever renumbers these the wrapper silently breaks — the
// CI drift check exists to catch that.
const SSR_VAULT_IX_LOCK_POSITION: u8 = 3;
const SSR_VAULT_IX_UNLOCK_POSITION: u8 = 4;

pinocchio::program_entrypoint!(process_instruction);
pinocchio::default_allocator!();
pinocchio::nostd_panic_handler!();

// ─── Instruction discriminators ─────────────────────────────────────────-

pub mod ix {
    /// Both parties sign, both compliance-check, both positions get
    /// locked, Repo PDA is created in `OPEN` state.
    pub const OPEN_REPO: u8 = 0;
    /// Borrower signs, both positions get unlocked, Repo PDA flips to
    /// `CLOSED`. Rejects after `expiry_slot`.
    pub const CLOSE_REPO: u8 = 1;
}

// ─── Custom error codes (0x4000-0x4FFF) ──────────────────────────────────

pub mod err {
    use pinocchio::program_error::ProgramError;

    // Account-shape failures.
    pub const REPO_PDA_MISMATCH: ProgramError = ProgramError::Custom(0x4001);
    pub const REPO_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x4002);
    pub const MISSING_SIGNER: ProgramError = ProgramError::Custom(0x4003);
    pub const INSTRUCTION_DATA_TOO_SHORT: ProgramError = ProgramError::Custom(0x4004);
    pub const VAULT_PROGRAM_MISMATCH: ProgramError = ProgramError::Custom(0x4005);
    pub const COMPLIANCE_PROGRAM_MISMATCH: ProgramError = ProgramError::Custom(0x4006);
    pub const BORROWER_RECORD_MISMATCH: ProgramError = ProgramError::Custom(0x4007);
    pub const LENDER_RECORD_MISMATCH: ProgramError = ProgramError::Custom(0x4008);

    // Compliance failures (mirror `CheckError`, distinct from earlier
    // programs at 0x10XX / 0x20XX / 0x30XX so wrapper-side logs stay
    // unambiguous).
    pub const COMPLIANCE_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x4010);
    pub const COMPLIANCE_STATUS_UNKNOWN: ProgramError = ProgramError::Custom(0x4011);
    pub const COMPLIANCE_UNVERIFIED: ProgramError = ProgramError::Custom(0x4012);
    pub const COMPLIANCE_SUSPENDED: ProgramError = ProgramError::Custom(0x4013);
    pub const COMPLIANCE_BLOCKED: ProgramError = ProgramError::Custom(0x4014);

    // Lifecycle.
    pub const ZERO_AMOUNT: ProgramError = ProgramError::Custom(0x4020);
    pub const NOT_OPEN: ProgramError = ProgramError::Custom(0x4021);
    pub const EXPIRED: ProgramError = ProgramError::Custom(0x4022);
}

// ─── Entrypoint dispatch ────────────────────────────────────────────────-

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (tag, rest) = data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;
    match *tag {
        ix::OPEN_REPO => open_repo(program_id, accounts, rest),
        ix::CLOSE_REPO => close_repo(program_id, accounts, rest),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

// ─── open_repo ──────────────────────────────────────────────────────────-

/// Open a bilateral lock. Account layout (Phase 3 minimum):
///
///   [0,  signer]            borrower
///   [1,  signer]            lender
///   [2,  signer, write]     payer (Repo PDA rent)
///   [3,  read]              borrower's `AccountRecord` PDA
///   [4,  read]              lender's `AccountRecord` PDA
///   [5,  read]              ssr_compliance_program (owner reference)
///   [6,  read]              ssr_vault_program (CPI target)
///   [7,  read]              collateral_vault PDA
///   [8,  write]             borrower's collateral Position PDA
///   [9,  read]              cash_vault PDA
///   [10, write]             lender's cash Position PDA
///   [11, write]             repo PDA (to be created)
///   [12, read]              system_program
///
/// Instruction data (after dispatch tag):
///   [0..8]    collateral_amount: u64 LE
///   [8..16]   cash_amount: u64 LE
///   [16..24]  expiry_slot: u64 LE (absolute slot)
///   [24..32]  nonce: u64 LE (disambiguates Repos sharing the other seeds)
#[allow(clippy::too_many_arguments)]
fn open_repo(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let [
        borrower_ai,
        lender_ai,
        payer_ai,
        borrower_record_ai,
        lender_record_ai,
        compliance_program_ai,
        vault_program_ai,
        collateral_vault_ai,
        collateral_position_ai,
        cash_vault_ai,
        cash_position_ai,
        repo_ai,
        _system_ai,
    ] = match accounts {
        [b, l, p, br, lr, cp, vp, cv, copos, cav, capos, r, s, ..] => {
            [b, l, p, br, lr, cp, vp, cv, copos, cav, capos, r, s]
        }
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 32 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let collateral_amount = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let cash_amount = u64::from_le_bytes(data[8..16].try_into().unwrap());
    let expiry_slot = u64::from_le_bytes(data[16..24].try_into().unwrap());
    let nonce = u64::from_le_bytes(data[24..32].try_into().unwrap());

    if collateral_amount == 0 || cash_amount == 0 {
        return Err(err::ZERO_AMOUNT);
    }
    require_signer(borrower_ai)?;
    require_signer(lender_ai)?;
    require_signer(payer_ai)?;
    // We do not assert `vault_program_ai.key()` matches a hard-coded
    // vault program ID — the wrapper is deployment-agnostic. The CPI
    // will fail loudly if the passed program does not understand our
    // `lock_position` / `unlock_position` discriminators, and
    // ssr-vault's own owner checks on the position PDA back-stop
    // accidental wrong-program calls.

    // Compliance checks against both parties' records.
    verify_record(borrower_record_ai, compliance_program_ai.key(), borrower_ai.key())
        .map_err(|e| translate_compliance_or(e, err::BORROWER_RECORD_MISMATCH))?;
    verify_record(lender_record_ai, compliance_program_ai.key(), lender_ai.key())
        .map_err(|e| translate_compliance_or(e, err::LENDER_RECORD_MISMATCH))?;

    // Derive the Repo PDA and assert match with the account passed in.
    let borrower_key = *borrower_ai.key();
    let lender_key = *lender_ai.key();
    let collateral_vault_key = *collateral_vault_ai.key();
    let cash_vault_key = *cash_vault_ai.key();
    let nonce_bytes = nonce.to_le_bytes();
    let (expected_repo, repo_bump) = find_program_address(
        &[
            seeds::REPO,
            &borrower_key,
            &lender_key,
            &collateral_vault_key,
            &cash_vault_key,
            &nonce_bytes,
        ],
        program_id,
    );
    if &expected_repo != repo_ai.key() {
        return Err(err::REPO_PDA_MISMATCH);
    }

    // CPI vault::lock_position for the borrower's collateral position.
    // The Repo PDA is the lock authority; vault stores it.
    cpi_lock_position(
        vault_program_ai.key(),
        borrower_ai,
        collateral_vault_ai,
        collateral_position_ai,
        compliance_program_ai,
        borrower_record_ai,
        collateral_amount,
        &expected_repo,
    )?;
    cpi_lock_position(
        vault_program_ai.key(),
        lender_ai,
        cash_vault_ai,
        cash_position_ai,
        compliance_program_ai,
        lender_record_ai,
        cash_amount,
        &expected_repo,
    )?;

    // Allocate the Repo PDA.
    let lamports = Rent::get()?.minimum_balance(Repo::LEN);
    let bump_seed = [repo_bump];
    let pda_seeds = [
        Seed::from(seeds::REPO),
        Seed::from(&borrower_key[..]),
        Seed::from(&lender_key[..]),
        Seed::from(&collateral_vault_key[..]),
        Seed::from(&cash_vault_key[..]),
        Seed::from(&nonce_bytes[..]),
        Seed::from(&bump_seed[..]),
    ];
    let pda_signer = Signer::from(&pda_seeds);
    CreateAccount {
        from: payer_ai,
        to: repo_ai,
        lamports,
        space: Repo::LEN as u64,
        owner: program_id,
    }
    .invoke_signed(&[pda_signer])?;

    let slot = Clock::get()?.slot;
    let mut rdata = repo_ai.try_borrow_mut_data()?;
    let r: &mut Repo = bytemuck::try_from_bytes_mut(&mut rdata[..Repo::LEN])
        .map_err(|_| err::REPO_LAYOUT_INVALID)?;
    *r = Repo::opened(
        borrower_key,
        lender_key,
        collateral_vault_key,
        cash_vault_key,
        collateral_amount,
        cash_amount,
        expiry_slot,
        nonce,
        slot,
        repo_bump,
    );

    msg!("ssr-repo: open_repo");
    Ok(())
}

// ─── close_repo ─────────────────────────────────────────────────────────-

/// Borrower-signed close: both positions get unlocked. Rejects after
/// `expiry_slot` — past expiry only the Phase 3b `default_repo` (not
/// implemented in Phase 3 minimum) is permitted.
///
///   [0,  signer]    borrower
///   [1,  read]      vault_program
///   [2,  read]      collateral_vault PDA
///   [3,  write]     borrower's collateral Position PDA
///   [4,  read]      cash_vault PDA
///   [5,  write]     lender's cash Position PDA
///   [6,  write]     repo PDA (writable for status flip)
fn close_repo(program_id: &Pubkey, accounts: &[AccountInfo], _data: &[u8]) -> ProgramResult {
    let [
        borrower_ai,
        vault_program_ai,
        collateral_vault_ai,
        collateral_position_ai,
        cash_vault_ai,
        cash_position_ai,
        repo_ai,
    ] = match accounts {
        [b, vp, cv, cpos, cav, capos, r, ..] => [b, vp, cv, cpos, cav, capos, r],
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    require_signer(borrower_ai)?;
    // We do not assert `vault_program_ai.key()` matches a hard-coded
    // vault program ID — the wrapper is deployment-agnostic. The CPI
    // will fail loudly if the passed program does not understand our
    // `lock_position` / `unlock_position` discriminators, and
    // ssr-vault's own owner checks on the position PDA back-stop
    // accidental wrong-program calls.

    // Decode Repo + assert PDA + lifecycle.
    let (collateral_amount, cash_amount, repo_bump, borrower_key, lender_key, collateral_vault_key, cash_vault_key, nonce) = {
        let rdata = repo_ai.try_borrow_data()?;
        if rdata.len() < Repo::LEN {
            return Err(err::REPO_LAYOUT_INVALID);
        }
        let r: &Repo = bytemuck::try_from_bytes(&rdata[..Repo::LEN])
            .map_err(|_| err::REPO_LAYOUT_INVALID)?;
        if r.status != repo_status::OPEN {
            return Err(err::NOT_OPEN);
        }
        if &r.borrower != borrower_ai.key() {
            return Err(err::BORROWER_RECORD_MISMATCH);
        }
        let now = Clock::get()?.slot;
        if now > r.expiry_slot {
            return Err(err::EXPIRED);
        }
        let nonce_bytes = r.nonce.to_le_bytes();
        let expected = create_program_address(
            &[
                seeds::REPO,
                &r.borrower,
                &r.lender,
                &r.collateral_vault,
                &r.cash_vault,
                &nonce_bytes,
                &[r.bump],
            ],
            program_id,
        )
        .map_err(|_| err::REPO_PDA_MISMATCH)?;
        if &expected != repo_ai.key() {
            return Err(err::REPO_PDA_MISMATCH);
        }
        (
            r.collateral_amount,
            r.cash_amount,
            r.bump,
            r.borrower,
            r.lender,
            r.collateral_vault,
            r.cash_vault,
            r.nonce,
        )
    };

    // CPI vault::unlock_position twice, signing as the Repo PDA.
    let nonce_bytes = nonce.to_le_bytes();
    let repo_bump_seed = [repo_bump];
    let repo_seeds = [
        Seed::from(seeds::REPO),
        Seed::from(&borrower_key[..]),
        Seed::from(&lender_key[..]),
        Seed::from(&collateral_vault_key[..]),
        Seed::from(&cash_vault_key[..]),
        Seed::from(&nonce_bytes[..]),
        Seed::from(&repo_bump_seed[..]),
    ];
    let repo_signer = Signer::from(&repo_seeds);
    cpi_unlock_position(
        vault_program_ai.key(),
        repo_ai,
        collateral_vault_ai,
        collateral_position_ai,
        collateral_amount,
        &repo_signer,
    )?;
    let repo_signer = Signer::from(&repo_seeds);
    cpi_unlock_position(
        vault_program_ai.key(),
        repo_ai,
        cash_vault_ai,
        cash_position_ai,
        cash_amount,
        &repo_signer,
    )?;

    let slot = Clock::get()?.slot;
    let mut rdata = repo_ai.try_borrow_mut_data()?;
    let r: &mut Repo = bytemuck::try_from_bytes_mut(&mut rdata[..Repo::LEN])
        .map_err(|_| err::REPO_LAYOUT_INVALID)?;
    r.status = repo_status::CLOSED;
    r.last_modified_slot = slot;

    msg!("ssr-repo: close_repo");
    Ok(())
}

// ─── Helpers ────────────────────────────────────────────────────────────-

fn require_signer(ai: &AccountInfo) -> ProgramResult {
    if !ai.is_signer() {
        return Err(err::MISSING_SIGNER);
    }
    Ok(())
}

fn verify_record(
    record_ai: &AccountInfo,
    compliance_program: &Pubkey,
    expected_participant: &Pubkey,
) -> Result<(), CheckErrorOrMismatch> {
    if record_ai.owner() != compliance_program {
        return Err(CheckErrorOrMismatch::Mismatch);
    }
    let data = record_ai.try_borrow_data().map_err(|_| CheckErrorOrMismatch::Mismatch)?;
    let record = ssr_types::read_account_record(&data).map_err(CheckErrorOrMismatch::Check)?;
    if &record.participant != expected_participant {
        return Err(CheckErrorOrMismatch::Mismatch);
    }
    record
        .check_transfer_allowed()
        .map_err(CheckErrorOrMismatch::Check)
}

enum CheckErrorOrMismatch {
    Check(CheckError),
    Mismatch,
}

fn translate_compliance_or(
    e: CheckErrorOrMismatch,
    mismatch: ProgramError,
) -> ProgramError {
    match e {
        CheckErrorOrMismatch::Check(c) => match c {
            CheckError::LayoutInvalid => err::COMPLIANCE_LAYOUT_INVALID,
            CheckError::StatusUnknown => err::COMPLIANCE_STATUS_UNKNOWN,
            CheckError::Unverified => err::COMPLIANCE_UNVERIFIED,
            CheckError::Suspended => err::COMPLIANCE_SUSPENDED,
            CheckError::Blocked => err::COMPLIANCE_BLOCKED,
        },
        CheckErrorOrMismatch::Mismatch => mismatch,
    }
}

#[allow(clippy::too_many_arguments)]
fn cpi_lock_position(
    vault_program_id: &Pubkey,
    depositor_ai: &AccountInfo,
    vault_ai: &AccountInfo,
    position_ai: &AccountInfo,
    compliance_program_ai: &AccountInfo,
    record_ai: &AccountInfo,
    amount: u64,
    lock_authority: &Pubkey,
) -> ProgramResult {
    let mut data = [0u8; 1 + 8 + 32];
    data[0] = SSR_VAULT_IX_LOCK_POSITION;
    data[1..9].copy_from_slice(&amount.to_le_bytes());
    data[9..41].copy_from_slice(lock_authority);

    let metas = [
        AccountMeta::new(depositor_ai.key(), false, true),
        AccountMeta::new(vault_ai.key(), false, false),
        AccountMeta::new(position_ai.key(), true, false),
        AccountMeta::new(compliance_program_ai.key(), false, false),
        AccountMeta::new(record_ai.key(), false, false),
    ];
    let ix = Instruction {
        program_id: vault_program_id,
        data: &data,
        accounts: &metas,
    };
    let infos = [depositor_ai, vault_ai, position_ai, compliance_program_ai, record_ai];
    pinocchio::cpi::slice_invoke_signed(&ix, &infos, &[])
}

fn cpi_unlock_position(
    vault_program_id: &Pubkey,
    authority_ai: &AccountInfo,
    vault_ai: &AccountInfo,
    position_ai: &AccountInfo,
    amount: u64,
    signer: &Signer<'_, '_>,
) -> ProgramResult {
    let mut data = [0u8; 1 + 8];
    data[0] = SSR_VAULT_IX_UNLOCK_POSITION;
    data[1..9].copy_from_slice(&amount.to_le_bytes());

    let metas = [
        AccountMeta::new(authority_ai.key(), false, true),
        AccountMeta::new(vault_ai.key(), false, false),
        AccountMeta::new(position_ai.key(), true, false),
    ];
    let ix = Instruction {
        program_id: vault_program_id,
        data: &data,
        accounts: &metas,
    };
    let infos = [authority_ai, vault_ai, position_ai];
    pinocchio::cpi::slice_invoke_signed(&ix, &infos, &[signer.clone()])
}

// ─── Test stubs ─────────────────────────────────────────────────────────-

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_in_repo_band() {
        let codes = [
            err::REPO_PDA_MISMATCH,
            err::NOT_OPEN,
            err::COMPLIANCE_BLOCKED,
            err::EXPIRED,
            err::ZERO_AMOUNT,
        ];
        for e in codes {
            if let ProgramError::Custom(code) = e {
                assert!(
                    (0x4001..=0x4FFF).contains(&code),
                    "code 0x{code:04X} outside repo band"
                );
            }
        }
    }
}
