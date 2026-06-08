//! `ssr-vault` — compliance-gated collateral vault primitive.
//!
//! One `Vault` PDA per `(deployment, asset_mint)` aggregates the
//! holdings of every depositor in that asset; one `Position` PDA per
//! `(vault, depositor)` tracks the per-depositor balance. The actual
//! Token-2022 holdings live in the vault PDA's canonical ATA (owner =
//! the vault PDA), not in the `Vault` account itself.
//!
//! Every `deposit` and `withdraw` verifies the depositor against the
//! `ssr-compliance` registry via the **composition wrapper pattern**
//! (read the `AccountRecord` PDA, confirm ownership + participant
//! field + `check_transfer_allowed`). The same pattern Phase 3+ repo /
//! lending / margin programs will use.
//!
//! Phase 2 surface:
//!   * `init_vault`     — admin allocates the `Vault` PDA. Client
//!                        separately creates the canonical ATA for the
//!                        vault PDA (standard SPL ATA flow).
//!   * `deposit`        — Token-2022 transfer depositor_ata → vault_ata,
//!                        signed by depositor; idempotently creates the
//!                        `Position` PDA on first call.
//!   * `withdraw`       — Token-2022 transfer vault_ata → depositor_ata,
//!                        signed by the vault PDA; rejects if amount
//!                        exceeds `position.available()`.

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
use ssr_types::{CheckError, Position, Vault, seeds};

// Gate the entrypoint + allocator + panic handler behind a feature so
// other on-chain programs that pull `ssr-vault` in as a library (for
// the `ix::*` discriminator constants, the `err::*` codes, etc.) don't
// double-declare these symbols at link time. Cargo's `no-entrypoint`
// feature is on by default for direct BPF builds; downstream lib
// consumers (e.g. `ssr-repo`) disable it.
#[cfg(not(feature = "no-entrypoint"))]
pinocchio::program_entrypoint!(process_instruction);
#[cfg(not(feature = "no-entrypoint"))]
pinocchio::default_allocator!();
#[cfg(not(feature = "no-entrypoint"))]
pinocchio::nostd_panic_handler!();

// ─── Constants ───────────────────────────────────────────────────────────

/// SPL Token-2022 program ID. Vaults only hold Token-2022 mints; if a
/// caller wants legacy SPL Token they can run a separate deployment.
pub const TOKEN_2022_PROGRAM_ID: Pubkey =
    pinocchio_pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// Token-2022 `TransferChecked` instruction discriminator.
const TOKEN_IX_TRANSFER_CHECKED: u8 = 12;
/// Length of the Token-2022 `TransferChecked` data payload.
const TRANSFER_CHECKED_DATA_LEN: usize = 1 + 8 + 1;

// ─── Instruction discriminators ──────────────────────────────────────────

pub mod ix {
    /// Allocate the `Vault` PDA for a given mint. Admin signer is
    /// recorded; client must separately create the canonical ATA.
    pub const INIT_VAULT: u8 = 0;
    /// Compliance-gated deposit. Idempotently creates the `Position` PDA.
    pub const DEPOSIT: u8 = 1;
    /// Compliance-gated withdrawal. Vault PDA signs the underlying
    /// Token-2022 transfer.
    pub const WITHDRAW: u8 = 2;
    /// Depositor consents to lock part of their position against an
    /// external `lock_authority` pubkey (Phase 3: the Repo PDA).
    pub const LOCK_POSITION: u8 = 3;
    /// `lock_authority` releases part of the lock. Signed by the
    /// authority's PDA via CPI seeds.
    pub const UNLOCK_POSITION: u8 = 4;
    /// Phase 3b: depositor moves part of their AVAILABLE balance
    /// (`amount_deposited - locked_amount`) to another position under
    /// the same vault. No Token-2022 movement — book-keeping only.
    pub const TRANSFER_WITHIN_VAULT: u8 = 5;
    /// Phase 3b: `lock_authority` moves part of the LOCKED balance to
    /// another position under the same vault. Used by `ssr-lending`
    /// at `liquidate_loan` to deliver the borrower's encumbered
    /// collateral to the lender after maturity. Decreases
    /// `from.locked_amount` and `from.amount_deposited` in lockstep;
    /// clears `from.lock_authority` if `from.locked_amount` reaches
    /// zero. Signed by the authority's PDA via CPI seeds.
    pub const SEIZE_LOCKED: u8 = 6;
    /// Phase 3b: idempotently allocate the depositor's `Position` PDA
    /// with zero balance, without any token movement. Lets the
    /// receiving side of a `transfer_within_vault` or `seize_locked`
    /// flow exist before the source-side operation runs. Does not bump
    /// the vault's `position_count` — that bump remains tied to the
    /// first `deposit` for compatibility with existing accounting.
    pub const INIT_POSITION: u8 = 7;
}

// ─── Custom error codes (0x3000-0x3FFF) ──────────────────────────────────

pub mod err {
    use pinocchio::program_error::ProgramError;

    // PDA / ownership shape failures.
    pub const VAULT_PDA_MISMATCH: ProgramError = ProgramError::Custom(0x3001);
    pub const POSITION_PDA_MISMATCH: ProgramError = ProgramError::Custom(0x3002);
    pub const RECORD_OWNER_MISMATCH: ProgramError = ProgramError::Custom(0x3003);
    pub const RECORD_PARTICIPANT_MISMATCH: ProgramError = ProgramError::Custom(0x3004);
    pub const VAULT_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x3005);
    pub const POSITION_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x3006);
    pub const MINT_MISMATCH: ProgramError = ProgramError::Custom(0x3007);
    pub const TOKEN_PROGRAM_MISMATCH: ProgramError = ProgramError::Custom(0x3008);
    pub const INSTRUCTION_DATA_TOO_SHORT: ProgramError = ProgramError::Custom(0x3009);
    pub const MISSING_SIGNER: ProgramError = ProgramError::Custom(0x300A);
    pub const MINT_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x300B);

    // Compliance failures (mirror `CheckError`, distinct from
    // ssr-compliance's 0x10XX and the wrapper's 0x20XX).
    pub const COMPLIANCE_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x3010);
    pub const COMPLIANCE_STATUS_UNKNOWN: ProgramError = ProgramError::Custom(0x3011);
    pub const COMPLIANCE_UNVERIFIED: ProgramError = ProgramError::Custom(0x3012);
    pub const COMPLIANCE_SUSPENDED: ProgramError = ProgramError::Custom(0x3013);
    pub const COMPLIANCE_BLOCKED: ProgramError = ProgramError::Custom(0x3014);

    // Withdraw-path arithmetic.
    pub const INSUFFICIENT_AVAILABLE: ProgramError = ProgramError::Custom(0x3020);
    pub const ZERO_AMOUNT: ProgramError = ProgramError::Custom(0x3021);

    // Lock / unlock.
    /// A different `lock_authority` already holds the position. The
    /// existing locker must fully release before another can claim.
    pub const LOCK_AUTHORITY_CONFLICT: ProgramError = ProgramError::Custom(0x3030);
    /// `unlock_position` signer does not match the stored `lock_authority`.
    pub const UNLOCK_AUTHORITY_MISMATCH: ProgramError = ProgramError::Custom(0x3031);
    /// `unlock_position` amount exceeds `position.locked_amount`.
    pub const UNLOCK_EXCEEDS_LOCKED: ProgramError = ProgramError::Custom(0x3032);

    // Transfer / seize (Phase 3b).
    /// The two positions point to different `vault` fields — moving
    /// balance between them would corrupt vault-level accounting.
    pub const POSITION_VAULT_MISMATCH: ProgramError = ProgramError::Custom(0x3040);
    /// `transfer_within_vault` signer is not the source position's
    /// depositor.
    pub const TRANSFER_DEPOSITOR_MISMATCH: ProgramError = ProgramError::Custom(0x3041);
    /// `seize_locked` signer does not match the source position's
    /// stored `lock_authority`.
    pub const SEIZE_AUTHORITY_MISMATCH: ProgramError = ProgramError::Custom(0x3042);
    /// `seize_locked` amount exceeds the source position's
    /// `locked_amount`.
    pub const SEIZE_EXCEEDS_LOCKED: ProgramError = ProgramError::Custom(0x3043);
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
        ix::INIT_VAULT => init_vault(program_id, accounts, rest),
        ix::DEPOSIT => deposit(program_id, accounts, rest),
        ix::WITHDRAW => withdraw(program_id, accounts, rest),
        ix::LOCK_POSITION => lock_position(program_id, accounts, rest),
        ix::UNLOCK_POSITION => unlock_position(program_id, accounts, rest),
        ix::TRANSFER_WITHIN_VAULT => transfer_within_vault(program_id, accounts, rest),
        ix::SEIZE_LOCKED => seize_locked(program_id, accounts, rest),
        ix::INIT_POSITION => init_position(program_id, accounts),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

// ─── init_vault ──────────────────────────────────────────────────────────

/// Allocate the per-mint `Vault` PDA and write its initial state.
///
/// Accounts:
///   [0, signer, write]  admin (becomes `Vault::admin`, also pays rent)
///   [1, write]          vault PDA (to be created at `[seeds::VAULT, mint]`)
///   [2, read]           mint (must be owned by Token-2022)
///   [3, read]           system_program
///
/// Instruction data (after the 1-byte discriminator):
///   [0..1] optional asset_class byte (see `ssr_types::asset_class`).
///          Omitted → defaults to `UNKNOWN`, which receives zero
///          credit in any margin view (`haircut_bps` returns 10_000).
fn init_vault(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let [admin_ai, vault_ai, mint_ai, _system_ai] = match accounts {
        [a, v, m, s, ..] => [a, v, m, s],
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    require_signer(admin_ai)?;
    if mint_ai.owner() != &TOKEN_2022_PROGRAM_ID {
        return Err(err::TOKEN_PROGRAM_MISMATCH);
    }
    let mint_key = *mint_ai.key();
    let (expected_vault, bump) = find_program_address(&[seeds::VAULT, &mint_key], program_id);
    if &expected_vault != vault_ai.key() {
        return Err(err::VAULT_PDA_MISMATCH);
    }

    let asset_class = data
        .first()
        .copied()
        .unwrap_or(ssr_types::asset_class::UNKNOWN);

    let lamports = Rent::get()?.minimum_balance(Vault::LEN);
    let bump_seed = [bump];
    let pda_seeds = [
        Seed::from(seeds::VAULT),
        Seed::from(&mint_key[..]),
        Seed::from(&bump_seed[..]),
    ];
    let pda_signer = Signer::from(&pda_seeds);
    CreateAccount {
        from: admin_ai,
        to: vault_ai,
        lamports,
        space: Vault::LEN as u64,
        owner: program_id,
    }
    .invoke_signed(&[pda_signer])?;

    let slot = Clock::get()?.slot;
    let mut data = vault_ai.try_borrow_mut_data()?;
    let v: &mut Vault = bytemuck::try_from_bytes_mut(&mut data[..Vault::LEN])
        .map_err(|_| err::VAULT_LAYOUT_INVALID)?;
    *v = Vault::initial(*admin_ai.key(), mint_key, slot, bump, asset_class);

    msg!("ssr-vault: init_vault");
    Ok(())
}

// ─── deposit ─────────────────────────────────────────────────────────────

/// Compliance-gated deposit. Idempotently creates the depositor's
/// `Position` PDA on first call.
///
/// Accounts:
///   [0,  signer]         depositor (owner of `depositor_ata`)
///   [1,  signer, write]  payer (typically same as depositor, but kept
///                         separate so a third party can fund the
///                         position-create rent without holding the
///                         depositor key)
///   [2,  read]           depositor's `AccountRecord` PDA
///   [3,  read]           ssr_compliance_program (owner reference)
///   [4,  write]          vault PDA
///   [5,  write]          position PDA
///   [6,  read]           mint
///   [7,  write]          depositor's ATA (source of transfer)
///   [8,  write]          vault's ATA       (dest of transfer)
///   [9,  read]           token_program (must equal Token-2022 ID)
///   [10, read]           system_program (for position-create rent CPI)
///
/// Instruction data (after the dispatch tag):
///   [0..8]  amount: u64 LE
fn deposit(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let [
        depositor_ai,
        payer_ai,
        record_ai,
        compliance_program_ai,
        vault_ai,
        position_ai,
        mint_ai,
        depositor_ata_ai,
        vault_ata_ai,
        token_program_ai,
        _system_ai,
    ] = match accounts {
        [d, p, r, cp, v, pos, m, da, va, tp, s, ..] => [d, p, r, cp, v, pos, m, da, va, tp, s],
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 8 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let amount = u64::from_le_bytes(data[..8].try_into().unwrap());
    if amount == 0 {
        return Err(err::ZERO_AMOUNT);
    }
    require_signer(depositor_ai)?;
    require_signer(payer_ai)?;
    if token_program_ai.key() != &TOKEN_2022_PROGRAM_ID {
        return Err(err::TOKEN_PROGRAM_MISMATCH);
    }

    // Vault + mint consistency.
    let (vault_data_mint, vault_bump) = read_vault(vault_ai, mint_ai, program_id)?;
    let _ = vault_data_mint;

    // Compliance check against the depositor's record.
    verify_record(record_ai, compliance_program_ai.key(), depositor_ai.key())?;

    // Idempotently create the position PDA on first deposit.
    let position_bump = ensure_position(
        program_id,
        payer_ai,
        position_ai,
        vault_ai,
        depositor_ai.key(),
    )?;

    // CPI Token-2022 transfer_checked from depositor_ata to vault_ata,
    // signed by the depositor (authority on the source).
    let decimals = read_mint_decimals(mint_ai)?;
    cpi_transfer_checked(
        token_program_ai.key(),
        depositor_ata_ai,
        mint_ai,
        vault_ata_ai,
        depositor_ai,
        amount,
        decimals,
        &[],
    )?;

    // Update vault + position bookkeeping.
    let slot = Clock::get()?.slot;
    {
        let mut data = vault_ai.try_borrow_mut_data()?;
        let v: &mut Vault = bytemuck::try_from_bytes_mut(&mut data[..Vault::LEN])
            .map_err(|_| err::VAULT_LAYOUT_INVALID)?;
        v.total_deposited = v.total_deposited.saturating_add(amount);
        v.last_modified_slot = slot;
        // We bump `position_count` exactly on the create path (signaled
        // by an `amount_deposited == 0` position we are about to write
        // into for the first time).
    }
    {
        let mut data = position_ai.try_borrow_mut_data()?;
        let p: &mut Position = bytemuck::try_from_bytes_mut(&mut data[..Position::LEN])
            .map_err(|_| err::POSITION_LAYOUT_INVALID)?;
        let was_empty = p.amount_deposited == 0 && p.locked_amount == 0;
        if was_empty {
            // First deposit: stamp the position's identity fields and
            // bump the vault's `position_count`.
            *p = Position::empty(*vault_ai.key(), *depositor_ai.key(), slot, position_bump);
            drop(data);
            let mut vdata = vault_ai.try_borrow_mut_data()?;
            let v: &mut Vault = bytemuck::try_from_bytes_mut(&mut vdata[..Vault::LEN])
                .map_err(|_| err::VAULT_LAYOUT_INVALID)?;
            v.position_count = v.position_count.saturating_add(1);
            drop(vdata);
            let mut pdata = position_ai.try_borrow_mut_data()?;
            let p: &mut Position = bytemuck::try_from_bytes_mut(&mut pdata[..Position::LEN])
                .map_err(|_| err::POSITION_LAYOUT_INVALID)?;
            p.amount_deposited = amount;
            p.last_modified_slot = slot;
        } else {
            p.amount_deposited = p.amount_deposited.saturating_add(amount);
            p.last_modified_slot = slot;
        }
    }

    let _ = vault_bump;
    msg!("ssr-vault: deposit");
    Ok(())
}

// ─── withdraw ────────────────────────────────────────────────────────────

/// Compliance-gated withdrawal. Vault PDA signs the underlying
/// Token-2022 transfer.
///
/// Accounts: same as `deposit`, except `payer` is dropped (no rent
/// movement) and the vault PDA must be the authority on the vault ATA.
///
///   [0,  signer]        depositor (recipient of the transfer)
///   [1,  read]          depositor's `AccountRecord` PDA
///   [2,  read]          ssr_compliance_program
///   [3,  write]         vault PDA (signs Token-2022 transfer)
///   [4,  write]         position PDA
///   [5,  read]          mint
///   [6,  write]         vault's ATA (source)
///   [7,  write]         depositor's ATA (dest)
///   [8,  read]          token_program (Token-2022)
///
/// Instruction data (after the dispatch tag):
///   [0..8]  amount: u64 LE
fn withdraw(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let [
        depositor_ai,
        record_ai,
        compliance_program_ai,
        vault_ai,
        position_ai,
        mint_ai,
        vault_ata_ai,
        depositor_ata_ai,
        token_program_ai,
    ] = match accounts {
        [d, r, cp, v, pos, m, va, da, tp, ..] => [d, r, cp, v, pos, m, va, da, tp],
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 8 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let amount = u64::from_le_bytes(data[..8].try_into().unwrap());
    if amount == 0 {
        return Err(err::ZERO_AMOUNT);
    }
    require_signer(depositor_ai)?;
    if token_program_ai.key() != &TOKEN_2022_PROGRAM_ID {
        return Err(err::TOKEN_PROGRAM_MISMATCH);
    }

    // Vault + position binding.
    let (_vault_mint, vault_bump) = read_vault(vault_ai, mint_ai, program_id)?;
    verify_position(
        position_ai,
        vault_ai.key(),
        depositor_ai.key(),
        program_id,
    )?;
    verify_record(record_ai, compliance_program_ai.key(), depositor_ai.key())?;

    // Pre-flight: enough available?
    {
        let pdata = position_ai.try_borrow_data()?;
        let p: &Position = bytemuck::try_from_bytes(&pdata[..Position::LEN])
            .map_err(|_| err::POSITION_LAYOUT_INVALID)?;
        if p.available() < amount {
            return Err(err::INSUFFICIENT_AVAILABLE);
        }
    }

    // CPI Token-2022 transfer_checked vault_ata → depositor_ata,
    // signed as the vault PDA.
    let mint_key = *mint_ai.key();
    let bump_seed = [vault_bump];
    let vault_seeds = [
        Seed::from(seeds::VAULT),
        Seed::from(&mint_key[..]),
        Seed::from(&bump_seed[..]),
    ];
    let vault_signer = Signer::from(&vault_seeds);
    let decimals = read_mint_decimals(mint_ai)?;
    cpi_transfer_checked(
        token_program_ai.key(),
        vault_ata_ai,
        mint_ai,
        depositor_ata_ai,
        vault_ai,
        amount,
        decimals,
        &[vault_signer],
    )?;

    // Update bookkeeping.
    let slot = Clock::get()?.slot;
    {
        let mut pdata = position_ai.try_borrow_mut_data()?;
        let p: &mut Position = bytemuck::try_from_bytes_mut(&mut pdata[..Position::LEN])
            .map_err(|_| err::POSITION_LAYOUT_INVALID)?;
        p.amount_deposited = p.amount_deposited.saturating_sub(amount);
        p.last_modified_slot = slot;
    }
    {
        let mut vdata = vault_ai.try_borrow_mut_data()?;
        let v: &mut Vault = bytemuck::try_from_bytes_mut(&mut vdata[..Vault::LEN])
            .map_err(|_| err::VAULT_LAYOUT_INVALID)?;
        v.total_deposited = v.total_deposited.saturating_sub(amount);
        v.last_modified_slot = slot;
    }

    msg!("ssr-vault: withdraw");
    Ok(())
}

// ─── lock_position ───────────────────────────────────────────────────────

/// Depositor consents to lock part of their position against an
/// external `lock_authority` (typically the calling program's PDA — for
/// Phase 3 the Repo PDA).
///
/// The position must be either *unlocked* or already locked by the
/// same `lock_authority` (additive). Mixing two authorities on one
/// position is rejected — the previous locker must fully release
/// (via `unlock_position` taking `locked_amount` to zero) before
/// another can claim.
///
/// Accounts:
///   [0, signer]   depositor (consents to lock their own collateral)
///   [1, read]     vault PDA
///   [2, write]    position PDA
///   [3, read]     ssr_compliance_program (owner reference)
///   [4, read]     depositor's `AccountRecord` PDA
///
/// Instruction data (after dispatch tag):
///   [0..8]    amount: u64 LE
///   [8..40]   lock_authority: [u8; 32]
fn lock_position(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let [
        depositor_ai,
        vault_ai,
        position_ai,
        compliance_program_ai,
        record_ai,
    ] = match accounts {
        [d, v, p, cp, r, ..] => [d, v, p, cp, r],
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 8 + 32 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let amount = u64::from_le_bytes(data[..8].try_into().unwrap());
    if amount == 0 {
        return Err(err::ZERO_AMOUNT);
    }
    let mut new_authority = [0u8; 32];
    new_authority.copy_from_slice(&data[8..40]);

    require_signer(depositor_ai)?;
    verify_record(record_ai, compliance_program_ai.key(), depositor_ai.key())?;
    verify_position(
        position_ai,
        vault_ai.key(),
        depositor_ai.key(),
        program_id,
    )?;

    let slot = Clock::get()?.slot;
    let mut pdata = position_ai.try_borrow_mut_data()?;
    let p: &mut Position = bytemuck::try_from_bytes_mut(&mut pdata[..Position::LEN])
        .map_err(|_| err::POSITION_LAYOUT_INVALID)?;
    if p.available() < amount {
        return Err(err::INSUFFICIENT_AVAILABLE);
    }
    if !p.is_unlocked() && p.lock_authority != new_authority {
        return Err(err::LOCK_AUTHORITY_CONFLICT);
    }
    p.locked_amount = p.locked_amount.saturating_add(amount);
    p.lock_authority = new_authority;
    p.last_modified_slot = slot;

    msg!("ssr-vault: lock_position");
    Ok(())
}

// ─── unlock_position ─────────────────────────────────────────────────────

/// `lock_authority` releases part of the lock. Signed by the authority's
/// PDA via CPI seeds — the runtime sets `authority_ai.is_signer()` true
/// because the calling program passed its signing seeds for that PDA.
///
/// Accounts:
///   [0, signer]   lock_authority (matches `position.lock_authority`)
///   [1, read]     vault PDA (just for binding consistency)
///   [2, write]    position PDA
///
/// Instruction data (after dispatch tag):
///   [0..8]    amount: u64 LE
fn unlock_position(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let [
        authority_ai,
        vault_ai,
        position_ai,
    ] = match accounts {
        [a, v, p, ..] => [a, v, p],
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 8 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let amount = u64::from_le_bytes(data[..8].try_into().unwrap());
    if amount == 0 {
        return Err(err::ZERO_AMOUNT);
    }
    require_signer(authority_ai)?;

    let slot = Clock::get()?.slot;
    let mut pdata = position_ai.try_borrow_mut_data()?;
    let p: &mut Position = bytemuck::try_from_bytes_mut(&mut pdata[..Position::LEN])
        .map_err(|_| err::POSITION_LAYOUT_INVALID)?;
    // Tight binding: the position must live under the vault account
    // passed in, and the position's stored lock_authority must match
    // the signer.
    if &p.vault != vault_ai.key() {
        return Err(err::POSITION_PDA_MISMATCH);
    }
    if &p.lock_authority != authority_ai.key() {
        return Err(err::UNLOCK_AUTHORITY_MISMATCH);
    }
    if amount > p.locked_amount {
        return Err(err::UNLOCK_EXCEEDS_LOCKED);
    }
    // Verify PDA derivation off the stored depositor + bump (we do
    // *not* require the depositor as an account here — the lock
    // authority is the on-chain consent path).
    let expected_pda = create_program_address(
        &[seeds::POSITION, &p.vault, &p.depositor, &[p.bump]],
        program_id,
    )
    .map_err(|_| err::POSITION_PDA_MISMATCH)?;
    if &expected_pda != position_ai.key() {
        return Err(err::POSITION_PDA_MISMATCH);
    }

    p.locked_amount -= amount;
    if p.locked_amount == 0 {
        p.lock_authority = [0u8; 32];
    }
    p.last_modified_slot = slot;

    msg!("ssr-vault: unlock_position");
    Ok(())
}

// ─── transfer_within_vault (Phase 3b) ────────────────────────────────────

/// Depositor moves AVAILABLE balance (`amount_deposited - locked_amount`)
/// from their position to another position under the same vault.
/// Book-keeping only — no Token-2022 movement. The vault's aggregate
/// `total_deposited` is invariant.
///
/// Accounts:
///   [0, signer]   from_depositor (= `from_position.depositor`)
///   [1, read]     vault PDA (both positions must reference this)
///   [2, write]    from_position PDA
///   [3, write]    to_position PDA
///
/// Instruction data (after dispatch tag):
///   [0..8]    amount: u64 LE
fn transfer_within_vault(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let [
        from_depositor_ai,
        vault_ai,
        from_position_ai,
        to_position_ai,
    ] = match accounts {
        [d, v, f, t, ..] => [d, v, f, t],
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 8 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let amount = u64::from_le_bytes(data[..8].try_into().unwrap());
    if amount == 0 {
        return Err(err::ZERO_AMOUNT);
    }
    require_signer(from_depositor_ai)?;
    // Source must match the depositor's PDA.
    verify_position(
        from_position_ai,
        vault_ai.key(),
        from_depositor_ai.key(),
        program_id,
    )?;

    // Read the destination's stored depositor before mutating, then
    // verify its PDA derivation using that depositor. We do NOT require
    // a separate depositor account for the destination — the position
    // already binds itself by its PDA seeds.
    let to_depositor: [u8; 32] = {
        let data = to_position_ai.try_borrow_data()?;
        if data.len() < Position::LEN {
            return Err(err::POSITION_LAYOUT_INVALID);
        }
        let p: &Position = bytemuck::try_from_bytes(&data[..Position::LEN])
            .map_err(|_| err::POSITION_LAYOUT_INVALID)?;
        p.depositor
    };
    verify_position(
        to_position_ai,
        vault_ai.key(),
        &to_depositor,
        program_id,
    )?;

    let slot = Clock::get()?.slot;
    // We need both positions mutable at once. Borrow them in turn —
    // they are distinct accounts so Pinocchio allows it.
    let mut from_data = from_position_ai.try_borrow_mut_data()?;
    let from_p: &mut Position = bytemuck::try_from_bytes_mut(&mut from_data[..Position::LEN])
        .map_err(|_| err::POSITION_LAYOUT_INVALID)?;
    if &from_p.vault != vault_ai.key() {
        return Err(err::POSITION_VAULT_MISMATCH);
    }
    if &from_p.depositor != from_depositor_ai.key() {
        return Err(err::TRANSFER_DEPOSITOR_MISMATCH);
    }
    if from_p.available() < amount {
        return Err(err::INSUFFICIENT_AVAILABLE);
    }
    from_p.amount_deposited -= amount;
    from_p.last_modified_slot = slot;
    drop(from_data);

    let mut to_data = to_position_ai.try_borrow_mut_data()?;
    let to_p: &mut Position = bytemuck::try_from_bytes_mut(&mut to_data[..Position::LEN])
        .map_err(|_| err::POSITION_LAYOUT_INVALID)?;
    if &to_p.vault != vault_ai.key() {
        return Err(err::POSITION_VAULT_MISMATCH);
    }
    to_p.amount_deposited = to_p.amount_deposited.saturating_add(amount);
    to_p.last_modified_slot = slot;

    msg!("ssr-vault: transfer_within_vault");
    Ok(())
}

// ─── seize_locked (Phase 3b) ─────────────────────────────────────────────

/// `lock_authority` moves LOCKED balance from one position to another
/// under the same vault. Decrements `from.locked_amount` and
/// `from.amount_deposited` together; clears `from.lock_authority` if
/// `from.locked_amount` reaches zero. Destination receives the amount
/// in its `amount_deposited` (unlocked — the new holder owns it
/// freely).
///
/// Accounts:
///   [0, signer]   authority (matches `from_position.lock_authority`)
///   [1, read]     vault PDA
///   [2, write]    from_position PDA (the encumbered source)
///   [3, write]    to_position PDA   (the seize destination)
///
/// Instruction data (after dispatch tag):
///   [0..8]    amount: u64 LE
fn seize_locked(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let [
        authority_ai,
        vault_ai,
        from_position_ai,
        to_position_ai,
    ] = match accounts {
        [a, v, f, t, ..] => [a, v, f, t],
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if data.len() < 8 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let amount = u64::from_le_bytes(data[..8].try_into().unwrap());
    if amount == 0 {
        return Err(err::ZERO_AMOUNT);
    }
    require_signer(authority_ai)?;

    // Verify destination position binds to the same vault and has a
    // matching PDA derivation.
    let to_depositor: [u8; 32] = {
        let data = to_position_ai.try_borrow_data()?;
        if data.len() < Position::LEN {
            return Err(err::POSITION_LAYOUT_INVALID);
        }
        let p: &Position = bytemuck::try_from_bytes(&data[..Position::LEN])
            .map_err(|_| err::POSITION_LAYOUT_INVALID)?;
        p.depositor
    };
    verify_position(
        to_position_ai,
        vault_ai.key(),
        &to_depositor,
        program_id,
    )?;

    let slot = Clock::get()?.slot;
    // Source side: assert the signer holds the lock and there is
    // enough locked balance.
    let mut from_data = from_position_ai.try_borrow_mut_data()?;
    let from_p: &mut Position = bytemuck::try_from_bytes_mut(&mut from_data[..Position::LEN])
        .map_err(|_| err::POSITION_LAYOUT_INVALID)?;
    if &from_p.vault != vault_ai.key() {
        return Err(err::POSITION_VAULT_MISMATCH);
    }
    if &from_p.lock_authority != authority_ai.key() {
        return Err(err::SEIZE_AUTHORITY_MISMATCH);
    }
    if amount > from_p.locked_amount {
        return Err(err::SEIZE_EXCEEDS_LOCKED);
    }
    // Verify PDA derivation off the stored depositor + bump.
    let expected_pda = create_program_address(
        &[seeds::POSITION, &from_p.vault, &from_p.depositor, &[from_p.bump]],
        program_id,
    )
    .map_err(|_| err::POSITION_PDA_MISMATCH)?;
    if &expected_pda != from_position_ai.key() {
        return Err(err::POSITION_PDA_MISMATCH);
    }
    from_p.amount_deposited -= amount;
    from_p.locked_amount -= amount;
    if from_p.locked_amount == 0 {
        from_p.lock_authority = [0u8; 32];
    }
    from_p.last_modified_slot = slot;
    drop(from_data);

    // Destination side: credit unlocked.
    let mut to_data = to_position_ai.try_borrow_mut_data()?;
    let to_p: &mut Position = bytemuck::try_from_bytes_mut(&mut to_data[..Position::LEN])
        .map_err(|_| err::POSITION_LAYOUT_INVALID)?;
    if &to_p.vault != vault_ai.key() {
        return Err(err::POSITION_VAULT_MISMATCH);
    }
    to_p.amount_deposited = to_p.amount_deposited.saturating_add(amount);
    to_p.last_modified_slot = slot;

    msg!("ssr-vault: seize_locked");
    Ok(())
}

// ─── init_position (Phase 3b) ────────────────────────────────────────────

/// Idempotently allocate the depositor's `Position` PDA with zero
/// balance. No token movement, no compliance check (the PDA holds no
/// value yet). Use this to pre-create the receiving side of a
/// `transfer_within_vault` or `seize_locked` before the source-side
/// signer can credit it.
///
/// Accounts:
///   [0, signer]         depositor (the position's `depositor` field)
///   [1, signer, write]  payer (funds rent if PDA is being created)
///   [2, read]           vault PDA
///   [3, write]          position PDA (to be created if absent)
///   [4, read]           system_program
fn init_position(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let [
        depositor_ai,
        payer_ai,
        vault_ai,
        position_ai,
        _system_ai,
    ] = match accounts {
        [d, p, v, pos, s, ..] => [d, p, v, pos, s],
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    require_signer(depositor_ai)?;
    require_signer(payer_ai)?;

    // Vault PDA must be one we own and is well-formed.
    {
        let data = vault_ai.try_borrow_data()?;
        if data.len() < Vault::LEN {
            return Err(err::VAULT_LAYOUT_INVALID);
        }
        let v: &Vault =
            bytemuck::try_from_bytes(&data[..Vault::LEN]).map_err(|_| err::VAULT_LAYOUT_INVALID)?;
        let expected =
            create_program_address(&[seeds::VAULT, &v.mint, &[v.bump]], program_id)
                .map_err(|_| err::VAULT_PDA_MISMATCH)?;
        if &expected != vault_ai.key() {
            return Err(err::VAULT_PDA_MISMATCH);
        }
    }

    // Was the position already there?
    let already_initialized = position_ai.owner() == program_id
        && position_ai
            .try_borrow_data()
            .map(|d| d.len() >= Position::LEN)
            .unwrap_or(false);

    let bump = ensure_position(
        program_id,
        payer_ai,
        position_ai,
        vault_ai,
        depositor_ai.key(),
    )?;

    if !already_initialized {
        // Newly created — stamp identity fields. Vault's position_count
        // is intentionally NOT bumped here; the bump remains tied to
        // the first `deposit` (see ix::INIT_POSITION docs).
        let slot = Clock::get()?.slot;
        let mut pdata = position_ai.try_borrow_mut_data()?;
        let p: &mut Position = bytemuck::try_from_bytes_mut(&mut pdata[..Position::LEN])
            .map_err(|_| err::POSITION_LAYOUT_INVALID)?;
        *p = Position::empty(*vault_ai.key(), *depositor_ai.key(), slot, bump);
    }

    msg!("ssr-vault: init_position");
    Ok(())
}

// ─── Helpers ─────────────────────────────────────────────────────────────

fn require_signer(ai: &AccountInfo) -> ProgramResult {
    if !ai.is_signer() {
        return Err(err::MISSING_SIGNER);
    }
    Ok(())
}

/// Read the vault and verify its PDA derivation + mint binding. Returns
/// `(mint_pubkey_inside_vault, bump)` for the caller to use.
fn read_vault(
    vault_ai: &AccountInfo,
    mint_ai: &AccountInfo,
    program_id: &Pubkey,
) -> Result<([u8; 32], u8), ProgramError> {
    let data = vault_ai.try_borrow_data()?;
    if data.len() < Vault::LEN {
        return Err(err::VAULT_LAYOUT_INVALID);
    }
    let v: &Vault =
        bytemuck::try_from_bytes(&data[..Vault::LEN]).map_err(|_| err::VAULT_LAYOUT_INVALID)?;
    if &v.mint != mint_ai.key() {
        return Err(err::MINT_MISMATCH);
    }
    let expected =
        create_program_address(&[seeds::VAULT, &v.mint, &[v.bump]], program_id)
            .map_err(|_| err::VAULT_PDA_MISMATCH)?;
    if &expected != vault_ai.key() {
        return Err(err::VAULT_PDA_MISMATCH);
    }
    Ok((v.mint, v.bump))
}

/// Verify the position PDA derivation + its `vault` / `depositor`
/// fields match the accounts passed in.
fn verify_position(
    position_ai: &AccountInfo,
    expected_vault: &Pubkey,
    expected_depositor: &Pubkey,
    program_id: &Pubkey,
) -> ProgramResult {
    let data = position_ai.try_borrow_data()?;
    if data.len() < Position::LEN {
        return Err(err::POSITION_LAYOUT_INVALID);
    }
    let p: &Position = bytemuck::try_from_bytes(&data[..Position::LEN])
        .map_err(|_| err::POSITION_LAYOUT_INVALID)?;
    if &p.vault != expected_vault {
        return Err(err::POSITION_PDA_MISMATCH);
    }
    if &p.depositor != expected_depositor {
        return Err(err::POSITION_PDA_MISMATCH);
    }
    let expected_pda = create_program_address(
        &[seeds::POSITION, expected_vault, expected_depositor, &[p.bump]],
        program_id,
    )
    .map_err(|_| err::POSITION_PDA_MISMATCH)?;
    if &expected_pda != position_ai.key() {
        return Err(err::POSITION_PDA_MISMATCH);
    }
    Ok(())
}

/// Compliance + identity check on an `AccountRecord` (composition
/// wrapper pattern, mirrors `ssr-dvp-wrapper::verify_record`).
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

/// Idempotently allocate the depositor's `Position` PDA. Returns the
/// position's bump in both the "newly created" and "already existed"
/// cases so the caller can use the same fields without branching.
fn ensure_position(
    program_id: &Pubkey,
    payer_ai: &AccountInfo,
    position_ai: &AccountInfo,
    vault_ai: &AccountInfo,
    depositor_key: &Pubkey,
) -> Result<u8, ProgramError> {
    // If the account already exists (owned by us, sized to Position::LEN),
    // pull the bump out and short-circuit.
    if position_ai.owner() == program_id {
        let data = position_ai.try_borrow_data()?;
        if data.len() >= Position::LEN {
            let p: &Position = bytemuck::try_from_bytes(&data[..Position::LEN])
                .map_err(|_| err::POSITION_LAYOUT_INVALID)?;
            return Ok(p.bump);
        }
    }
    // Otherwise create it.
    let vault_key = *vault_ai.key();
    let (expected_pda, bump) = find_program_address(
        &[seeds::POSITION, &vault_key, depositor_key],
        program_id,
    );
    if &expected_pda != position_ai.key() {
        return Err(err::POSITION_PDA_MISMATCH);
    }
    let lamports = Rent::get()?.minimum_balance(Position::LEN);
    let bump_seed = [bump];
    let pda_seeds = [
        Seed::from(seeds::POSITION),
        Seed::from(&vault_key[..]),
        Seed::from(&depositor_key[..]),
        Seed::from(&bump_seed[..]),
    ];
    let pda_signer = Signer::from(&pda_seeds);
    CreateAccount {
        from: payer_ai,
        to: position_ai,
        lamports,
        space: Position::LEN as u64,
        owner: program_id,
    }
    .invoke_signed(&[pda_signer])?;
    Ok(bump)
}

/// Read a Token-2022 mint's `decimals` field. The base SPL Mint layout
/// (which Token-2022 inherits) places it at offset 44:
///   `mint_authority: COption<Pubkey>` (4 + 32 = 36)
///   `supply: u64`                     (8)        ← 36..44
///   `decimals: u8`                    (1)        ← 44
fn read_mint_decimals(mint_ai: &AccountInfo) -> Result<u8, ProgramError> {
    let data = mint_ai.try_borrow_data()?;
    if data.len() < 45 {
        return Err(err::MINT_LAYOUT_INVALID);
    }
    Ok(data[44])
}

/// Construct + invoke a Token-2022 `TransferChecked` CPI.
///
/// `signers` is empty for depositor-authorized transfers; for the
/// vault-authorized withdraw path the caller passes the vault PDA's
/// signer seeds.
#[allow(clippy::too_many_arguments)]
fn cpi_transfer_checked(
    token_program_id: &Pubkey,
    source: &AccountInfo,
    mint: &AccountInfo,
    destination: &AccountInfo,
    authority: &AccountInfo,
    amount: u64,
    decimals: u8,
    signers: &[Signer<'_, '_>],
) -> ProgramResult {
    let mut data = [0u8; TRANSFER_CHECKED_DATA_LEN];
    data[0] = TOKEN_IX_TRANSFER_CHECKED;
    data[1..9].copy_from_slice(&amount.to_le_bytes());
    data[9] = decimals;

    let metas = [
        AccountMeta::new(source.key(), true, false),
        AccountMeta::new(mint.key(), false, false),
        AccountMeta::new(destination.key(), true, false),
        AccountMeta::new(authority.key(), false, true),
    ];
    let ix = Instruction {
        program_id: token_program_id,
        data: &data,
        accounts: &metas,
    };
    let infos = [source, mint, destination, authority];
    pinocchio::cpi::slice_invoke_signed(&ix, &infos, signers)
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_error_mapping_is_distinct_and_in_vault_band() {
        let mapped = [
            check_error_to_program_error(CheckError::LayoutInvalid),
            check_error_to_program_error(CheckError::StatusUnknown),
            check_error_to_program_error(CheckError::Unverified),
            check_error_to_program_error(CheckError::Suspended),
            check_error_to_program_error(CheckError::Blocked),
        ];
        for i in 0..mapped.len() {
            for j in (i + 1)..mapped.len() {
                assert_ne!(mapped[i], mapped[j], "{i} and {j} collide");
            }
        }
        for e in mapped {
            if let ProgramError::Custom(code) = e {
                assert!(
                    code >= 0x3010 && code < 0x3020,
                    "code 0x{code:04X} outside vault compliance band"
                );
            } else {
                panic!("expected Custom variant");
            }
        }
    }

    #[test]
    fn vault_compliance_band_does_not_overlap_other_programs() {
        // ssr-compliance: 0x1001-0x102B
        // ssr-dvp-wrapper: 0x2001-0x2014
        // ssr-vault:       0x3001-0x3014
        let our_codes = [
            err::VAULT_PDA_MISMATCH,
            err::INSUFFICIENT_AVAILABLE,
            err::COMPLIANCE_BLOCKED,
            err::ZERO_AMOUNT,
        ];
        for e in our_codes {
            if let ProgramError::Custom(code) = e {
                assert!(
                    (0x3001..=0x3FFF).contains(&code),
                    "code 0x{code:04X} outside vault band"
                );
            }
        }
    }
}
