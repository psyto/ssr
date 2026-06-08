//! `ssr-lending` — compliance-gated bilateral collateralized term loan.
//!
//! Phase 3b model (cash flows enforced on-chain):
//!
//!  * `open_loan` — borrower posts collateral (locked against the
//!    `Loan` PDA), lender's `principal_amount` is transferred from
//!    their cash position to the borrower's cash position via the
//!    vault's `transfer_within_vault` primitive. Both legs settle in
//!    the same transaction. `interest_bps_per_year` is recorded for
//!    deterministic interest computation at repay.
//!  * `repay_loan` — before `maturity_slot`: borrower transfers
//!    `principal_amount + accrued_simple_interest` from their cash
//!    position back to the lender's cash position, then the locked
//!    collateral is released. `Loan::status` → `REPAID`.
//!  * `liquidate_loan` — after `maturity_slot` with no repay: the
//!    lender invokes the `Loan` PDA's authority to `seize_locked` the
//!    borrower's encumbered collateral straight into the lender's own
//!    collateral position. `Loan::status` → `LIQUIDATED`.
//!
//! Simple interest model: `interest = principal * slots_elapsed *
//! interest_bps_per_year / (SLOTS_PER_YEAR * 10_000)`. `SLOTS_PER_YEAR`
//! is held at the documented Solana mainnet target of 78_840_000
//! (~0.4 s per slot). Slot-cadence drift on the live cluster will
//! cause the realized rate to differ from the named bps figure; for
//! contracts that need precise yield, off-chain settlement is still
//! the right venue. The on-chain figure is the deterministic,
//! verifiable bound.

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
use ssr_types::{
    CheckError, Loan, LoanList, Position, PriceFeed, RiskParams, Vault, loan_status, seeds,
};

// Mirror of `ssr_vault::ix::*`. See `Cargo.toml` for why we duplicate
// rather than depend; if `ssr-vault` ever renumbers these the wrapper
// silently breaks — the drift-check test in `tests/e2e_lending.rs`
// exists to catch that.
const SSR_VAULT_IX_LOCK_POSITION: u8 = 3;
const SSR_VAULT_IX_UNLOCK_POSITION: u8 = 4;
const SSR_VAULT_IX_TRANSFER_WITHIN_VAULT: u8 = 5;
const SSR_VAULT_IX_SEIZE_LOCKED: u8 = 6;

// Slot cadence at the documented Solana mainnet target of ~0.4 s per
// slot: 365 * 24 * 3600 / 0.4 = 78_840_000. Drift between this number
// and the live cluster's realized cadence is documented at the
// instruction-level — see `repay_loan` for the trade-off.
//
// Public so off-chain tooling (`ssr-cli margin show`) can mirror
// `repay_loan`'s interest formula exactly when projecting liabilities.
pub const SLOTS_PER_YEAR: u128 = 78_840_000;
pub const BPS_DENOMINATOR: u128 = 10_000;

pinocchio::program_entrypoint!(process_instruction);
pinocchio::default_allocator!();
pinocchio::nostd_panic_handler!();

// ─── Instruction discriminators ─────────────────────────────────────────-

pub mod ix {
    /// Both parties sign, both compliance-check, borrower's collateral
    /// is locked against the `Loan` PDA, and `principal_amount` is
    /// transferred from lender's cash position to borrower's cash
    /// position. Loan PDA is created in `OPEN` state.
    pub const OPEN_LOAN: u8 = 0;
    /// Borrower signs. Before `maturity_slot`: transfers `principal +
    /// accrued_interest` from borrower's cash position back to the
    /// lender's cash position, then unlocks the borrower's collateral.
    /// `Loan::status` flips to `REPAID`. Past `maturity_slot` this
    /// rejects — the lender uses `liquidate_loan` instead.
    pub const REPAY_LOAN: u8 = 1;
    /// Lender signs. After `maturity_slot` with `Loan::status ==
    /// OPEN`: the `Loan` PDA seizes the borrower's locked collateral
    /// and credits it (unlocked) into the lender's collateral
    /// position. `Loan::status` flips to `LIQUIDATED`.
    pub const LIQUIDATE_LOAN: u8 = 2;
}

// ─── Custom error codes (0x5000-0x5FFF) ──────────────────────────────────

pub mod err {
    use pinocchio::program_error::ProgramError;

    // Account-shape failures.
    pub const LOAN_PDA_MISMATCH: ProgramError = ProgramError::Custom(0x5001);
    pub const LOAN_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x5002);
    pub const MISSING_SIGNER: ProgramError = ProgramError::Custom(0x5003);
    pub const INSTRUCTION_DATA_TOO_SHORT: ProgramError = ProgramError::Custom(0x5004);
    pub const VAULT_PROGRAM_MISMATCH: ProgramError = ProgramError::Custom(0x5005);
    pub const COMPLIANCE_PROGRAM_MISMATCH: ProgramError = ProgramError::Custom(0x5006);
    pub const BORROWER_RECORD_MISMATCH: ProgramError = ProgramError::Custom(0x5007);
    pub const LENDER_RECORD_MISMATCH: ProgramError = ProgramError::Custom(0x5008);

    // Compliance failures (mirror `CheckError`, distinct from earlier
    // programs at 0x10XX / 0x20XX / 0x30XX / 0x40XX so wrapper-side
    // logs stay unambiguous).
    pub const COMPLIANCE_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x5010);
    pub const COMPLIANCE_STATUS_UNKNOWN: ProgramError = ProgramError::Custom(0x5011);
    pub const COMPLIANCE_UNVERIFIED: ProgramError = ProgramError::Custom(0x5012);
    pub const COMPLIANCE_SUSPENDED: ProgramError = ProgramError::Custom(0x5013);
    pub const COMPLIANCE_BLOCKED: ProgramError = ProgramError::Custom(0x5014);

    // Lifecycle.
    pub const ZERO_AMOUNT: ProgramError = ProgramError::Custom(0x5020);
    pub const NOT_OPEN: ProgramError = ProgramError::Custom(0x5021);
    pub const MATURED: ProgramError = ProgramError::Custom(0x5022);
    /// `liquidate_loan` invoked while the loan is still inside the
    /// repayment window (`slot <= maturity_slot`).
    pub const NOT_MATURED: ProgramError = ProgramError::Custom(0x5023);
    /// `repay_loan` was called by an account whose signer pubkey does
    /// not match the loan's stored `borrower`, or `liquidate_loan` by
    /// an account that does not match the loan's stored `lender`.
    pub const SIGNER_NOT_PARTY: ProgramError = ProgramError::Custom(0x5024);
    /// Interest accrual overflowed 64 bits — the `(principal,
    /// bps, slots)` combination is outside the supported range.
    /// Surfaced rather than silently saturated so off-chain
    /// settlement can decide how to handle the edge.
    pub const INTEREST_OVERFLOW: ProgramError = ProgramError::Custom(0x5025);

    // Phase 4 v1b — LoanList maintenance + margin enforcement.
    /// `LoanList` PDA passed in does not derive from
    /// `[seeds::LOAN_LIST, borrower] @ program_id`.
    pub const LOAN_LIST_PDA_MISMATCH: ProgramError = ProgramError::Custom(0x5030);
    /// `LoanList` account data is shorter than `LoanList::LEN` or
    /// misaligned for Pod cast.
    pub const LOAN_LIST_LAYOUT_INVALID: ProgramError = ProgramError::Custom(0x5031);
    /// `LoanList::borrower` does not match the borrower this handler
    /// is operating on. Catches a borrower passing somebody else's
    /// loan-list account.
    pub const LOAN_LIST_BORROWER_MISMATCH: ProgramError = ProgramError::Custom(0x5032);
    /// `LoanList` is at `MAX_ENTRIES`; the borrower must repay or
    /// have a loan liquidated before opening a new one.
    pub const LOAN_LIST_FULL: ProgramError = ProgramError::Custom(0x5033);
    /// `repay_loan` / `liquidate_loan` was invoked on a `Loan` that
    /// isn't in the borrower's `LoanList`. Either the list is corrupt
    /// or this loan was opened pre-v1b — surface explicitly so an
    /// operator can decide.
    pub const LOAN_NOT_IN_LIST: ProgramError = ProgramError::Custom(0x5034);
    /// `open_loan`'s margin pre-check determined that opening this
    /// loan would leave the borrower with negative net margin
    /// (haircut-adjusted available collateral < existing + new
    /// liabilities projected to their maturities).
    pub const MARGIN_INSUFFICIENT: ProgramError = ProgramError::Custom(0x5035);
    /// `RiskParams` PDA passed in does not derive from
    /// `[seeds::RISK_PARAMS] @ compliance_program`.
    pub const RISK_PARAMS_PDA_MISMATCH: ProgramError = ProgramError::Custom(0x5036);
    /// A Position PDA passed in the margin pre-check list does not
    /// have `Position::depositor` equal to the borrower, or its
    /// `Position::vault` does not match the paired vault account.
    pub const MARGIN_POSITION_MISMATCH: ProgramError = ProgramError::Custom(0x5037);
    /// The set of existing-loan PDAs passed for the margin check does
    /// not match `LoanList` exactly (size or membership). The borrower
    /// can't selectively hide liabilities — the gate requires the
    /// full set.
    pub const MARGIN_LOAN_SET_MISMATCH: ProgramError = ProgramError::Custom(0x5038);

    // Phase 4 v1d — oracle-priced cross-margin.
    /// `enforce_margin` couldn't find a `PriceFeed` matching one of
    /// the mints in scope (a position's vault mint or a loan's
    /// cash-vault mint). The caller must pass a `PriceFeed` PDA for
    /// every distinct mint that contributes to the margin
    /// calculation.
    pub const PRICE_FEED_MISSING: ProgramError = ProgramError::Custom(0x5039);
    /// A `PriceFeed` is older than `RiskParams.max_staleness_slots`.
    /// Refresh via `ssr-cli compliance update-price` and retry; or,
    /// for emergencies, governance can widen the staleness gate via
    /// `set_max_staleness`.
    pub const PRICE_FEED_STALE: ProgramError = ProgramError::Custom(0x503A);
    /// Price-feed slice violated the strictly-ascending pubkey
    /// invariant (caller-supplied; required for cheap dedup of
    /// distinct mints).
    pub const PRICE_FEED_ORDER_INVALID: ProgramError = ProgramError::Custom(0x503B);
    /// Intermediate `balance × price × credit` arithmetic overflowed
    /// u128. Defensive backstop for pathological inputs; real-world
    /// vaults won't hit this.
    pub const PRICE_OVERFLOW: ProgramError = ProgramError::Custom(0x503C);
    /// `PriceFeed` PDA passed in does not derive from `[seeds::PRICE_FEED,
    /// mint] @ compliance_program`, or its data shape is invalid.
    pub const PRICE_FEED_PDA_MISMATCH: ProgramError = ProgramError::Custom(0x503D);
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
        ix::OPEN_LOAN => open_loan(program_id, accounts, rest),
        ix::REPAY_LOAN => repay_loan(program_id, accounts, rest),
        ix::LIQUIDATE_LOAN => liquidate_loan(program_id, accounts, rest),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

// ─── open_loan ──────────────────────────────────────────────────────────-

/// Open a collateralized term loan with on-chain cash drawdown.
///
/// Accounts:
///   [0,  signer]            borrower
///   [1,  signer]            lender
///   [2,  signer, write]     payer (Loan PDA + LoanList rent)
///   [3,  read]              borrower's `AccountRecord` PDA
///   [4,  read]              lender's `AccountRecord` PDA
///   [5,  read]              ssr_compliance_program (owner reference)
///   [6,  read]              ssr_vault_program (CPI target)
///   [7,  read]              collateral_vault PDA
///   [8,  write]             borrower's collateral Position PDA
///   [9,  read]              cash_vault PDA
///   [10, write]             lender's cash Position PDA (drawdown src)
///   [11, write]             borrower's cash Position PDA (drawdown dst)
///   [12, write]             loan PDA (to be created)
///   [13, read]              system_program
///   [14, write]             borrower's `LoanList` PDA (allocated on
///                           first use, appended every subsequent open)
///   [15, read]              `RiskParams` PDA (read for haircut table)
///   [16..16+2*N, read]      `extra_positions_count = N` pairs of
///                           (additional position PDA, vault PDA) the
///                           borrower wants to count toward their
///                           cross-margin pool. Each position is
///                           validated to belong to the borrower.
///   [16+2*N..16+2*N+M, read] `M = LoanList::count` existing-loan PDAs
///                           in `LoanList.entries` order. The handler
///                           rejects if this set does not match
///                           `LoanList` exactly.
///   [16+2*N..16+2*N+M, read] `M = existing_loans_count` existing-loan
///                           PDAs in `LoanList.entries` order.
///   [16+2*N+M..16+2*N+2*M, read] `M` cash_vault PDAs, one per loan
///                           above in the same order (Phase 4 v1e —
///                           lets the gate look up each loan's cash
///                           mint independently for multi-cash-mint
///                           cross-margin).
///   [16+2*N+2*M.., read]    `price_feed_count = P` `PriceFeed` PDAs,
///                           one per distinct mint involved in the
///                           margin check (collateral mint, new cash
///                           mint, each extra position's vault mint,
///                           each existing loan's cash-vault mint).
///                           Must be in strictly-ascending pubkey
///                           order for cheap dedup.
///
/// Instruction data (after dispatch tag):
///   [0..8]    collateral_amount: u64 LE
///   [8..16]   principal_amount: u64 LE
///   [16..24]  maturity_slot: u64 LE (absolute slot)
///   [24..32]  nonce: u64 LE (disambiguates Loans sharing the other seeds)
///   [32..36]  interest_bps_per_year: u32 LE
///   [36]      extra_positions_count: u8 (Phase 4 v1b — `N` above)
///   [37]      price_feed_count: u8 (Phase 4 v1d — `P` above)
///   [38]      existing_loans_count: u8 (Phase 4 v1e — `M` above;
///             must equal `LoanList.count` or handler rejects with
///             `MARGIN_LOAN_SET_MISMATCH`)
#[allow(clippy::too_many_arguments)]
fn open_loan(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let fixed: &[&AccountInfo; 16] = match accounts {
        [b, l, p, br, lr, cp, vp, cv, copos, cav, lcpos, bcpos, ln, s, ll, rp, ..] => {
            &[b, l, p, br, lr, cp, vp, cv, copos, cav, lcpos, bcpos, ln, s, ll, rp]
        }
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    let borrower_ai = fixed[0];
    let lender_ai = fixed[1];
    let payer_ai = fixed[2];
    let borrower_record_ai = fixed[3];
    let lender_record_ai = fixed[4];
    let compliance_program_ai = fixed[5];
    let vault_program_ai = fixed[6];
    let collateral_vault_ai = fixed[7];
    let collateral_position_ai = fixed[8];
    let cash_vault_ai = fixed[9];
    let lender_cash_position_ai = fixed[10];
    let borrower_cash_position_ai = fixed[11];
    let loan_ai = fixed[12];
    let _system_ai = fixed[13];
    let loan_list_ai = fixed[14];
    let risk_params_ai = fixed[15];

    if data.len() < 39 {
        return Err(err::INSTRUCTION_DATA_TOO_SHORT);
    }
    let collateral_amount = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let principal_amount = u64::from_le_bytes(data[8..16].try_into().unwrap());
    let maturity_slot = u64::from_le_bytes(data[16..24].try_into().unwrap());
    let nonce = u64::from_le_bytes(data[24..32].try_into().unwrap());
    let interest_bps_per_year = u32::from_le_bytes(data[32..36].try_into().unwrap());
    let extra_positions_count = data[36] as usize;
    let price_feed_count = data[37] as usize;
    let existing_loans_count = data[38] as usize;

    // Split trailing accounts into four sections in order:
    //   (a) extra_positions_count pairs of (position, vault),
    //   (b) existing_loans_count loan PDAs,
    //   (c) existing_loans_count cash_vault PDAs (parallel to (b);
    //       Phase 4 v1e — lets the gate look up each loan's mint),
    //   (d) price_feed_count PriceFeed PDAs.
    let trailing = &accounts[16..];
    let positions_section_len = extra_positions_count.checked_mul(2)
        .ok_or(ProgramError::InvalidInstructionData)?;
    let cash_vaults_section_len = existing_loans_count;
    let total_needed = positions_section_len
        .checked_add(existing_loans_count).ok_or(ProgramError::InvalidInstructionData)?
        .checked_add(cash_vaults_section_len).ok_or(ProgramError::InvalidInstructionData)?
        .checked_add(price_feed_count).ok_or(ProgramError::InvalidInstructionData)?;
    if trailing.len() < total_needed {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    let extra_position_pairs = &trailing[..positions_section_len];
    let loans_start = positions_section_len;
    let cash_vaults_start = loans_start + existing_loans_count;
    let price_feeds_start = cash_vaults_start + cash_vaults_section_len;
    let existing_loan_ais = &trailing[loans_start..cash_vaults_start];
    let existing_loan_cash_vault_ais = &trailing[cash_vaults_start..price_feeds_start];
    let price_feed_ais = &trailing[price_feeds_start..price_feeds_start + price_feed_count];

    if collateral_amount == 0 || principal_amount == 0 {
        return Err(err::ZERO_AMOUNT);
    }
    require_signer(borrower_ai)?;
    require_signer(lender_ai)?;
    require_signer(payer_ai)?;
    // We do not assert `vault_program_ai.key()` matches a hard-coded
    // vault program ID — the wrapper is deployment-agnostic. The CPI
    // will fail loudly if the passed program does not understand our
    // `lock_position` / `transfer_within_vault` discriminators, and
    // ssr-vault's own owner checks on the position PDAs back-stop
    // accidental wrong-program calls.

    // Compliance checks against both parties' records.
    verify_record(borrower_record_ai, compliance_program_ai.key(), borrower_ai.key())
        .map_err(|e| translate_compliance_or(e, err::BORROWER_RECORD_MISMATCH))?;
    verify_record(lender_record_ai, compliance_program_ai.key(), lender_ai.key())
        .map_err(|e| translate_compliance_or(e, err::LENDER_RECORD_MISMATCH))?;

    // Derive the Loan PDA and assert match with the account passed in.
    let borrower_key = *borrower_ai.key();
    let lender_key = *lender_ai.key();
    let collateral_vault_key = *collateral_vault_ai.key();
    let cash_vault_key = *cash_vault_ai.key();
    let nonce_bytes = nonce.to_le_bytes();
    let (expected_loan, loan_bump) = find_program_address(
        &[
            seeds::LOAN,
            &borrower_key,
            &lender_key,
            &collateral_vault_key,
            &cash_vault_key,
            &nonce_bytes,
        ],
        program_id,
    );
    if &expected_loan != loan_ai.key() {
        return Err(err::LOAN_PDA_MISMATCH);
    }

    // Phase 4 v1b: margin pre-check. Compute the borrower's post-open
    // net margin from accounts the caller passed and reject if it
    // would go negative. Runs BEFORE any CPI / CreateAccount so a
    // failure leaves no state changes to unwind. See the
    // `enforce_margin` doc-comment for the model.
    let now_slot = Clock::get()?.slot;
    enforce_margin(
        program_id,
        compliance_program_ai,
        risk_params_ai,
        &borrower_key,
        collateral_vault_ai,
        collateral_position_ai,
        cash_vault_ai,
        borrower_cash_position_ai,
        loan_list_ai,
        extra_position_pairs,
        existing_loan_ais,
        existing_loan_cash_vault_ais,
        price_feed_ais,
        collateral_amount,
        principal_amount,
        maturity_slot,
        interest_bps_per_year,
        now_slot,
    )?;

    // CPI vault::lock_position for the borrower's collateral position
    // — the Loan PDA is the lock authority.
    cpi_lock_position(
        vault_program_ai.key(),
        borrower_ai,
        collateral_vault_ai,
        collateral_position_ai,
        compliance_program_ai,
        borrower_record_ai,
        collateral_amount,
        &expected_loan,
    )?;
    // CPI vault::transfer_within_vault from lender's cash position to
    // borrower's cash position. The lender signs.
    cpi_transfer_within_vault(
        vault_program_ai.key(),
        lender_ai,
        cash_vault_ai,
        lender_cash_position_ai,
        borrower_cash_position_ai,
        principal_amount,
    )?;

    // Allocate the Loan PDA.
    let lamports = Rent::get()?.minimum_balance(Loan::LEN);
    let bump_seed = [loan_bump];
    let pda_seeds = [
        Seed::from(seeds::LOAN),
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
        to: loan_ai,
        lamports,
        space: Loan::LEN as u64,
        owner: program_id,
    }
    .invoke_signed(&[pda_signer])?;

    let slot = Clock::get()?.slot;
    let mut ldata = loan_ai.try_borrow_mut_data()?;
    let l: &mut Loan = bytemuck::try_from_bytes_mut(&mut ldata[..Loan::LEN])
        .map_err(|_| err::LOAN_LAYOUT_INVALID)?;
    *l = Loan::opened(
        borrower_key,
        lender_key,
        collateral_vault_key,
        cash_vault_key,
        collateral_amount,
        principal_amount,
        maturity_slot,
        nonce,
        interest_bps_per_year,
        slot,
        loan_bump,
    );
    // Drop the mutable borrow before LoanList alloc-or-append, which
    // borrows a different account but shares the runtime borrow
    // bookkeeping in some pinocchio versions.
    drop(ldata);

    // Phase 4 v1b: keep the borrower's authoritative open-loan index
    // in lockstep with `Loan::status`. First open allocates the PDA,
    // every subsequent open appends. The handler refuses to open a
    // 17th simultaneous loan (`LOAN_LIST_FULL`); the borrower must
    // repay or have a loan liquidated first.
    alloc_or_append_loan_list(program_id, payer_ai, borrower_ai, loan_list_ai, expected_loan)?;

    msg!("ssr-lending: open_loan");
    Ok(())
}

// ─── repay_loan ─────────────────────────────────────────────────────────-

/// Borrower-signed repay before `maturity_slot`. Transfers `principal
/// + accrued_simple_interest` from borrower's cash position to lender's
/// cash position, then unlocks the borrower's collateral. Rejects past
/// `maturity_slot` — the lender uses `liquidate_loan` instead.
///
/// Accounts:
///   [0, signer]    borrower
///   [1, read]      vault_program (CPI target)
///   [2, read]      collateral_vault PDA
///   [3, write]     borrower's collateral Position PDA (unlock target)
///   [4, read]      cash_vault PDA
///   [5, write]     borrower's cash Position PDA (transfer source)
///   [6, write]     lender's cash Position PDA   (transfer destination)
///   [7, write]     loan PDA (writable for status flip)
///   [8, write]     borrower's `LoanList` PDA (entry is removed)
fn repay_loan(program_id: &Pubkey, accounts: &[AccountInfo], _data: &[u8]) -> ProgramResult {
    let [
        borrower_ai,
        vault_program_ai,
        collateral_vault_ai,
        collateral_position_ai,
        cash_vault_ai,
        borrower_cash_position_ai,
        lender_cash_position_ai,
        loan_ai,
        loan_list_ai,
    ] = match accounts {
        [b, vp, cv, cpos, cav, bcpos, lcpos, ln, ll, ..] => {
            [b, vp, cv, cpos, cav, bcpos, lcpos, ln, ll]
        }
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    require_signer(borrower_ai)?;
    // We do not assert `vault_program_ai.key()` matches a hard-coded
    // vault program ID — the wrapper is deployment-agnostic. The CPI
    // will fail loudly if the passed program does not understand our
    // discriminators, and ssr-vault's own owner checks on the position
    // PDAs back-stop accidental wrong-program calls.

    // Decode Loan + assert PDA + lifecycle, then compute the
    // repayment amount before any CPIs run.
    let (
        collateral_amount,
        principal_amount,
        loan_bump,
        borrower_key,
        lender_key,
        collateral_vault_key,
        cash_vault_key,
        nonce,
        opened_slot,
        interest_bps_per_year,
        now_slot,
    ) = {
        let ldata = loan_ai.try_borrow_data()?;
        if ldata.len() < Loan::LEN {
            return Err(err::LOAN_LAYOUT_INVALID);
        }
        let l: &Loan = bytemuck::try_from_bytes(&ldata[..Loan::LEN])
            .map_err(|_| err::LOAN_LAYOUT_INVALID)?;
        if l.status != loan_status::OPEN {
            return Err(err::NOT_OPEN);
        }
        if &l.borrower != borrower_ai.key() {
            return Err(err::SIGNER_NOT_PARTY);
        }
        let now = Clock::get()?.slot;
        if now > l.maturity_slot {
            return Err(err::MATURED);
        }
        let nonce_bytes = l.nonce.to_le_bytes();
        let expected = create_program_address(
            &[
                seeds::LOAN,
                &l.borrower,
                &l.lender,
                &l.collateral_vault,
                &l.cash_vault,
                &nonce_bytes,
                &[l.bump],
            ],
            program_id,
        )
        .map_err(|_| err::LOAN_PDA_MISMATCH)?;
        if &expected != loan_ai.key() {
            return Err(err::LOAN_PDA_MISMATCH);
        }
        (
            l.collateral_amount,
            l.principal_amount,
            l.bump,
            l.borrower,
            l.lender,
            l.collateral_vault,
            l.cash_vault,
            l.nonce,
            l.opened_slot,
            l.interest_bps_per_year,
            now,
        )
    };

    let slots_elapsed = now_slot.saturating_sub(opened_slot);
    let interest =
        compute_simple_interest(principal_amount, interest_bps_per_year, slots_elapsed)?;
    let total_due = principal_amount
        .checked_add(interest)
        .ok_or(err::INTEREST_OVERFLOW)?;

    // CPI vault::transfer_within_vault from borrower's cash position
    // to lender's cash position (borrower signs).
    cpi_transfer_within_vault(
        vault_program_ai.key(),
        borrower_ai,
        cash_vault_ai,
        borrower_cash_position_ai,
        lender_cash_position_ai,
        total_due,
    )?;

    // CPI vault::unlock_position to release the borrower's collateral,
    // signing as the Loan PDA.
    let nonce_bytes = nonce.to_le_bytes();
    let loan_bump_seed = [loan_bump];
    let loan_seeds = [
        Seed::from(seeds::LOAN),
        Seed::from(&borrower_key[..]),
        Seed::from(&lender_key[..]),
        Seed::from(&collateral_vault_key[..]),
        Seed::from(&cash_vault_key[..]),
        Seed::from(&nonce_bytes[..]),
        Seed::from(&loan_bump_seed[..]),
    ];
    let loan_signer = Signer::from(&loan_seeds);
    cpi_unlock_position(
        vault_program_ai.key(),
        loan_ai,
        collateral_vault_ai,
        collateral_position_ai,
        collateral_amount,
        &loan_signer,
    )?;

    let slot = Clock::get()?.slot;
    {
        let mut ldata = loan_ai.try_borrow_mut_data()?;
        let l: &mut Loan = bytemuck::try_from_bytes_mut(&mut ldata[..Loan::LEN])
            .map_err(|_| err::LOAN_LAYOUT_INVALID)?;
        l.status = loan_status::REPAID;
        l.last_modified_slot = slot;
    }

    // Phase 4 v1b: drop this loan from the borrower's open-loan
    // index. `remove_from_loan_list` verifies the PDA via the stored
    // bump so a swapped-in list can't trick us into mutating an
    // arbitrary account.
    remove_from_loan_list(program_id, borrower_ai.key(), loan_list_ai, loan_ai.key())?;

    msg!("ssr-lending: repay_loan");
    Ok(())
}

// ─── liquidate_loan ─────────────────────────────────────────────────────-

/// Lender-signed post-maturity claim. Seizes the borrower's encumbered
/// collateral into the lender's collateral position. Rejects if the
/// loan is still within its repayment window — `repay_loan` is the
/// borrower's remedy there.
///
/// Accounts:
///   [0, signer]   lender
///   [1, read]     vault_program (CPI target)
///   [2, read]     collateral_vault PDA
///   [3, write]    borrower's collateral Position PDA (seize source)
///   [4, write]    lender's collateral Position PDA   (seize destination)
///   [5, write]    loan PDA (writable for status flip)
///   [6, write]    borrower's `LoanList` PDA (entry is removed)
fn liquidate_loan(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    _data: &[u8],
) -> ProgramResult {
    let [
        lender_ai,
        vault_program_ai,
        collateral_vault_ai,
        borrower_collateral_ai,
        lender_collateral_ai,
        loan_ai,
        loan_list_ai,
    ] = match accounts {
        [l, vp, cv, bc, lc, ln, ll, ..] => [l, vp, cv, bc, lc, ln, ll],
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    require_signer(lender_ai)?;

    let (
        collateral_amount,
        loan_bump,
        borrower_key,
        lender_key,
        collateral_vault_key,
        cash_vault_key,
        nonce,
    ) = {
        let ldata = loan_ai.try_borrow_data()?;
        if ldata.len() < Loan::LEN {
            return Err(err::LOAN_LAYOUT_INVALID);
        }
        let l: &Loan = bytemuck::try_from_bytes(&ldata[..Loan::LEN])
            .map_err(|_| err::LOAN_LAYOUT_INVALID)?;
        if l.status != loan_status::OPEN {
            return Err(err::NOT_OPEN);
        }
        if &l.lender != lender_ai.key() {
            return Err(err::SIGNER_NOT_PARTY);
        }
        let now = Clock::get()?.slot;
        if now <= l.maturity_slot {
            return Err(err::NOT_MATURED);
        }
        let nonce_bytes = l.nonce.to_le_bytes();
        let expected = create_program_address(
            &[
                seeds::LOAN,
                &l.borrower,
                &l.lender,
                &l.collateral_vault,
                &l.cash_vault,
                &nonce_bytes,
                &[l.bump],
            ],
            program_id,
        )
        .map_err(|_| err::LOAN_PDA_MISMATCH)?;
        if &expected != loan_ai.key() {
            return Err(err::LOAN_PDA_MISMATCH);
        }
        (
            l.collateral_amount,
            l.bump,
            l.borrower,
            l.lender,
            l.collateral_vault,
            l.cash_vault,
            l.nonce,
        )
    };

    // CPI vault::seize_locked, Loan PDA signs as the lock authority.
    let nonce_bytes = nonce.to_le_bytes();
    let loan_bump_seed = [loan_bump];
    let loan_seeds = [
        Seed::from(seeds::LOAN),
        Seed::from(&borrower_key[..]),
        Seed::from(&lender_key[..]),
        Seed::from(&collateral_vault_key[..]),
        Seed::from(&cash_vault_key[..]),
        Seed::from(&nonce_bytes[..]),
        Seed::from(&loan_bump_seed[..]),
    ];
    let loan_signer = Signer::from(&loan_seeds);
    cpi_seize_locked(
        vault_program_ai.key(),
        loan_ai,
        collateral_vault_ai,
        borrower_collateral_ai,
        lender_collateral_ai,
        collateral_amount,
        &loan_signer,
    )?;

    let slot = Clock::get()?.slot;
    {
        let mut ldata = loan_ai.try_borrow_mut_data()?;
        let l: &mut Loan = bytemuck::try_from_bytes_mut(&mut ldata[..Loan::LEN])
            .map_err(|_| err::LOAN_LAYOUT_INVALID)?;
        l.status = loan_status::LIQUIDATED;
        l.last_modified_slot = slot;
    }

    // Phase 4 v1b: drop the entry from the borrower's `LoanList`.
    // The borrower didn't sign — `liquidate_loan` is lender-driven —
    // but the borrower pubkey is authoritatively recorded in the
    // `Loan` struct (`borrower_key`, decoded above and PDA-verified).
    remove_from_loan_list(program_id, &borrower_key, loan_list_ai, loan_ai.key())?;

    msg!("ssr-lending: liquidate_loan");
    Ok(())
}

/// Simple linear interest computation. Returns the accrued u64 amount,
/// or `INTEREST_OVERFLOW` if the intermediate `principal * bps * slots`
/// product exceeds u128, or the final result exceeds u64.
fn compute_simple_interest(
    principal: u64,
    bps_per_year: u32,
    slots_elapsed: u64,
) -> Result<u64, ProgramError> {
    if bps_per_year == 0 || slots_elapsed == 0 {
        return Ok(0);
    }
    let p = principal as u128;
    let r = bps_per_year as u128;
    let t = slots_elapsed as u128;
    let numerator = p
        .checked_mul(r)
        .and_then(|x| x.checked_mul(t))
        .ok_or(err::INTEREST_OVERFLOW)?;
    let denom = SLOTS_PER_YEAR
        .checked_mul(BPS_DENOMINATOR)
        .ok_or(err::INTEREST_OVERFLOW)?;
    let interest = numerator / denom;
    if interest > u64::MAX as u128 {
        return Err(err::INTEREST_OVERFLOW);
    }
    Ok(interest as u64)
}

// ─── Helpers ────────────────────────────────────────────────────────────-

fn require_signer(ai: &AccountInfo) -> ProgramResult {
    if !ai.is_signer() {
        return Err(err::MISSING_SIGNER);
    }
    Ok(())
}

/// Allocate the borrower's `LoanList` PDA on first use, then append
/// `loan_pda`. Borrower-side rent comes from `payer_ai`.
fn alloc_or_append_loan_list(
    program_id: &Pubkey,
    payer_ai: &AccountInfo,
    borrower_ai: &AccountInfo,
    loan_list_ai: &AccountInfo,
    loan_pda: [u8; 32],
) -> ProgramResult {
    let borrower_key = *borrower_ai.key();
    let (expected_pda, bump) =
        find_program_address(&[seeds::LOAN_LIST, &borrower_key], program_id);
    if &expected_pda != loan_list_ai.key() {
        return Err(err::LOAN_LIST_PDA_MISMATCH);
    }

    // First-time allocation when the PDA hasn't been created yet
    // (pinocchio surfaces that as `data.len() == 0`; lamports may
    // already match the rent floor if the runtime pre-funded for
    // CreateAccount).
    let needs_alloc = loan_list_ai.try_borrow_data().map(|d| d.len() == 0).unwrap_or(true);
    if needs_alloc {
        let lamports = Rent::get()?.minimum_balance(LoanList::LEN);
        let bump_seed = [bump];
        let pda_seeds = [
            Seed::from(seeds::LOAN_LIST),
            Seed::from(&borrower_key[..]),
            Seed::from(&bump_seed[..]),
        ];
        let pda_signer = Signer::from(&pda_seeds);
        CreateAccount {
            from: payer_ai,
            to: loan_list_ai,
            lamports,
            space: LoanList::LEN as u64,
            owner: program_id,
        }
        .invoke_signed(&[pda_signer])?;
        let slot = Clock::get()?.slot;
        let mut data = loan_list_ai.try_borrow_mut_data()?;
        let ll: &mut LoanList = bytemuck::try_from_bytes_mut(&mut data[..LoanList::LEN])
            .map_err(|_| err::LOAN_LIST_LAYOUT_INVALID)?;
        *ll = LoanList::empty(borrower_key, slot, bump);
        if !ll.push(loan_pda) {
            return Err(err::LOAN_LIST_FULL);
        }
        return Ok(());
    }

    let mut data = loan_list_ai.try_borrow_mut_data()?;
    if data.len() < LoanList::LEN {
        return Err(err::LOAN_LIST_LAYOUT_INVALID);
    }
    let ll: &mut LoanList = bytemuck::try_from_bytes_mut(&mut data[..LoanList::LEN])
        .map_err(|_| err::LOAN_LIST_LAYOUT_INVALID)?;
    if ll.borrower != borrower_key {
        return Err(err::LOAN_LIST_BORROWER_MISMATCH);
    }
    if !ll.push(loan_pda) {
        return Err(err::LOAN_LIST_FULL);
    }
    ll.last_modified_slot = Clock::get()?.slot;
    Ok(())
}

/// Phase 4 v1e margin enforcement gate. Computes the borrower's
/// **post-open** net margin from the caller-passed account set,
/// converted to micro-USD via `PriceFeed`s, and rejects with
/// `MARGIN_INSUFFICIENT` if it would go negative. v1e extends v1d
/// to multi-cash-mint: each existing loan's `cash_vault` is passed
/// in a parallel slice so the gate can look up that loan's mint's
/// feed independently — borrowers can hold simultaneous loans in
/// different settlement currencies off the same collateral.
///
/// Model (conservative cross-margin, oracle-priced):
///
///   For each position the borrower discloses:
///     usd = balance × price(mint) × (10_000 − haircut(asset_class))
///           / (10^mint_decimals × 10_000)
///
///   pool_usd = Σ position_usd
///            − collateral_amount × price(collateral_mint)
///              × collateral_credit / (10^collateral_decimals × 10_000)
///              (the new lock — the locked collateral leaves the pool
///               as freely-available cash equivalent)
///
///   For each existing open loan (must match `LoanList` exactly):
///     liab_usd = (principal + interest_to_maturity) × price(cash_mint)
///                / 10^cash_decimals
///
///   new_liab_usd = (new_principal + new_interest_to_maturity)
///                  × price(new_cash_mint) / 10^new_cash_decimals
///
///   reject if pool_usd < existing_liab_usd + new_liab_usd
///
/// The new cash drawdown deliberately does NOT add to the pool —
/// see the v1b note on conservative accounting: the borrower can
/// withdraw drawn cash and leave the obligation behind.
///
/// Oracle-related defenses:
/// * Every distinct mint in scope must have a `PriceFeed` in the
///   caller-supplied price-feed slice. Missing feed →
///   `PRICE_FEED_MISSING`.
/// * Each feed's `last_updated_slot` is checked against
///   `RiskParams.max_staleness_slots`. Stale → `PRICE_FEED_STALE`.
/// * Feeds in the slice are validated against
///   `[seeds::PRICE_FEED, mint] @ compliance_program` and owner;
///   the slice must be in strictly-ascending pubkey order to defeat
///   substitution / duplicate stuffing.
///
/// Pre-v1d adversarial defenses (carried forward):
/// * Strictly-ascending pubkey order on extra positions, plus
///   position ownership + vault checks (see `add_position_to_pool`).
/// * Existing-loan set must match `LoanList` exactly — no omission,
///   no substitution, no duplication.
#[allow(clippy::too_many_arguments)]
fn enforce_margin(
    program_id: &Pubkey,
    compliance_program_ai: &AccountInfo,
    risk_params_ai: &AccountInfo,
    borrower_key: &Pubkey,
    collateral_vault_ai: &AccountInfo,
    collateral_position_ai: &AccountInfo,
    cash_vault_ai: &AccountInfo,
    borrower_cash_position_ai: &AccountInfo,
    loan_list_ai: &AccountInfo,
    extra_position_pairs: &[AccountInfo],
    existing_loan_ais: &[AccountInfo],
    existing_loan_cash_vault_ais: &[AccountInfo],
    price_feed_ais: &[AccountInfo],
    new_collateral_amount: u64,
    new_principal_amount: u64,
    new_maturity_slot: u64,
    new_interest_bps_per_year: u32,
    now_slot: u64,
) -> ProgramResult {
    // Phase 4 v1e: per-loan cash_vault slice must match the loan
    // slice 1:1 — they're parallel arrays.
    if existing_loan_cash_vault_ais.len() != existing_loan_ais.len() {
        return Err(err::MARGIN_LOAN_SET_MISMATCH);
    }
    // ── 1. Read RiskParams (haircut table + max_staleness).
    let (expected_risk, _) = find_program_address(
        &[seeds::RISK_PARAMS],
        compliance_program_ai.key(),
    );
    if &expected_risk != risk_params_ai.key() {
        return Err(err::RISK_PARAMS_PDA_MISMATCH);
    }
    if risk_params_ai.owner() != compliance_program_ai.key() {
        return Err(err::RISK_PARAMS_PDA_MISMATCH);
    }
    let risk: RiskParams = {
        let data = risk_params_ai.try_borrow_data()?;
        if data.len() < RiskParams::LEN {
            return Err(err::RISK_PARAMS_PDA_MISMATCH);
        }
        *bytemuck::try_from_bytes::<RiskParams>(&data[..RiskParams::LEN])
            .map_err(|_| err::RISK_PARAMS_PDA_MISMATCH)?
    };

    // ── 2. Validate the price-feed slice once: strictly-ascending
    //    pubkey order (dedup invariant), each feed owned by the
    //    compliance program at the expected PDA, freshness gate.
    //    Build a tiny in-memory table of (mint → (price, decimals))
    //    by scanning the slice in order.
    let mut last_seen: [u8; 32] = [0u8; 32];
    let mut have_last = false;
    for feed_ai in price_feed_ais {
        let key: &[u8; 32] = feed_ai.key();
        if have_last && key <= &last_seen {
            return Err(err::PRICE_FEED_ORDER_INVALID);
        }
        last_seen = *key;
        have_last = true;
        if feed_ai.owner() != compliance_program_ai.key() {
            return Err(err::PRICE_FEED_PDA_MISMATCH);
        }
        let data = feed_ai.try_borrow_data()?;
        if data.len() < PriceFeed::LEN {
            return Err(err::PRICE_FEED_PDA_MISMATCH);
        }
        let pf: &PriceFeed = bytemuck::try_from_bytes(&data[..PriceFeed::LEN])
            .map_err(|_| err::PRICE_FEED_PDA_MISMATCH)?;
        let expected = create_program_address(
            &[seeds::PRICE_FEED, &pf.mint, &[pf.bump]],
            compliance_program_ai.key(),
        )
        .map_err(|_| err::PRICE_FEED_PDA_MISMATCH)?;
        if &expected != feed_ai.key() {
            return Err(err::PRICE_FEED_PDA_MISMATCH);
        }
        // Staleness gate: 0 disables (pre-v1d behavior).
        if risk.max_staleness_slots > 0
            && now_slot.saturating_sub(pf.last_updated_slot) > risk.max_staleness_slots
        {
            return Err(err::PRICE_FEED_STALE);
        }
    }

    // ── 3. Walk the borrower's positions, converting each to micro-
    //    USD via its mint's PriceFeed and the haircut from RiskParams.
    let collateral_mint = read_vault_mint(collateral_vault_ai)?;
    let cash_mint = read_vault_mint(cash_vault_ai)?;
    let collateral_feed = find_feed_for_mint(price_feed_ais, &collateral_mint)?;
    let cash_feed = find_feed_for_mint(price_feed_ais, &cash_mint)?;
    let collateral_class = read_vault_asset_class(collateral_vault_ai)?;
    let collateral_credit =
        (10_000u128).saturating_sub(risk.haircut_for(collateral_class) as u128);
    let cash_class = read_vault_asset_class(cash_vault_ai)?;
    let cash_credit = (10_000u128).saturating_sub(risk.haircut_for(cash_class) as u128);

    let mut pool_usd: u128 = 0;
    let mut seen_positions: [[u8; 32]; LoanList::MAX_ENTRIES + 2] =
        [[0u8; 32]; LoanList::MAX_ENTRIES + 2];
    let mut seen_count: usize = 0;

    pool_usd = pool_usd.saturating_add(add_position_usd(
        collateral_position_ai,
        collateral_vault_ai,
        borrower_key,
        &collateral_feed,
        collateral_credit,
        &mut seen_positions,
        &mut seen_count,
    )?);
    pool_usd = pool_usd.saturating_add(add_position_usd(
        borrower_cash_position_ai,
        cash_vault_ai,
        borrower_key,
        &cash_feed,
        cash_credit,
        &mut seen_positions,
        &mut seen_count,
    )?);

    // Extra positions — caller-disclosed, strictly-ascending pubkey
    // order required to defeat duplicate-stuffing.
    if extra_position_pairs.len() % 2 != 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let mut last_extra: [u8; 32] = [0u8; 32];
    let mut have_last_extra = false;
    for pair in extra_position_pairs.chunks(2) {
        let pos_ai = &pair[0];
        let vault_ai = &pair[1];
        let pos_key: &[u8; 32] = pos_ai.key();
        if have_last_extra && pos_key <= &last_extra {
            return Err(err::MARGIN_POSITION_MISMATCH);
        }
        last_extra = *pos_key;
        have_last_extra = true;
        let class = read_vault_asset_class(vault_ai)?;
        let credit = (10_000u128).saturating_sub(risk.haircut_for(class) as u128);
        let mint = read_vault_mint(vault_ai)?;
        let feed = find_feed_for_mint(price_feed_ais, &mint)?;
        pool_usd = pool_usd.saturating_add(add_position_usd(
            pos_ai,
            vault_ai,
            borrower_key,
            &feed,
            credit,
            &mut seen_positions,
            &mut seen_count,
        )?);
    }

    // ── 4. Post-open delta: subtract the lock value (haircut-adjusted
    //    micro-USD of `new_collateral_amount`).
    let collateral_drag = balance_to_micro_usd(
        new_collateral_amount,
        collateral_feed.price_micro_usd,
        collateral_feed.mint_decimals,
        collateral_credit,
    )?;
    pool_usd = pool_usd.saturating_sub(collateral_drag);
    // The new drawdown is intentionally NOT added — same conservative
    // model rationale as v1b.

    // ── 5. Sum existing liabilities. LoanList membership check is
    //    identical to v1b; what's new in v1d is converting each
    //    `principal + interest` to micro-USD via the cash-vault mint's
    //    PriceFeed.
    let (expected_loan_list_pda, _) =
        find_program_address(&[seeds::LOAN_LIST, borrower_key], program_id);
    if &expected_loan_list_pda != loan_list_ai.key() {
        return Err(err::LOAN_LIST_PDA_MISMATCH);
    }
    let (loan_list_entries_count, loan_list_entries) = {
        let data = loan_list_ai.try_borrow_data()?;
        if data.len() == 0 {
            (0usize, [[0u8; 32]; LoanList::MAX_ENTRIES])
        } else {
            if data.len() < LoanList::LEN {
                return Err(err::LOAN_LIST_LAYOUT_INVALID);
            }
            let ll: &LoanList = bytemuck::try_from_bytes(&data[..LoanList::LEN])
                .map_err(|_| err::LOAN_LIST_LAYOUT_INVALID)?;
            if &ll.borrower != borrower_key {
                return Err(err::LOAN_LIST_BORROWER_MISMATCH);
            }
            let count = ll.count as usize;
            let mut copy = [[0u8; 32]; LoanList::MAX_ENTRIES];
            copy[..count].copy_from_slice(&ll.entries[..count]);
            (count, copy)
        }
    };
    if existing_loan_ais.len() != loan_list_entries_count {
        return Err(err::MARGIN_LOAN_SET_MISMATCH);
    }
    let mut covered: [bool; LoanList::MAX_ENTRIES] = [false; LoanList::MAX_ENTRIES];
    let mut existing_liab_usd: u128 = 0;
    for (idx, loan_ai) in existing_loan_ais.iter().enumerate() {
        let loan_key: &[u8; 32] = loan_ai.key();
        let mut found = false;
        for (i, entry) in loan_list_entries[..loan_list_entries_count].iter().enumerate() {
            if entry == loan_key {
                if covered[i] {
                    return Err(err::MARGIN_LOAN_SET_MISMATCH);
                }
                covered[i] = true;
                found = true;
                break;
            }
        }
        if !found {
            return Err(err::MARGIN_LOAN_SET_MISMATCH);
        }
        let data = loan_ai.try_borrow_data()?;
        if data.len() < Loan::LEN {
            return Err(err::LOAN_LAYOUT_INVALID);
        }
        let l: &Loan = bytemuck::try_from_bytes(&data[..Loan::LEN])
            .map_err(|_| err::LOAN_LAYOUT_INVALID)?;
        if &l.borrower != borrower_key {
            return Err(err::MARGIN_LOAN_SET_MISMATCH);
        }
        // Phase 4 v1e: each loan's cash_vault is passed in the
        // parallel slice. Validate it matches the loan's recorded
        // cash_vault (otherwise caller could swap in a feed for a
        // cheaper mint and underprice the liability), read its mint,
        // and look up the corresponding PriceFeed. Liabilities don't
        // take a haircut.
        let loan_cash_vault_ai = &existing_loan_cash_vault_ais[idx];
        if &l.cash_vault != loan_cash_vault_ai.key() {
            return Err(err::MARGIN_LOAN_SET_MISMATCH);
        }
        let loan_cash_mint = read_vault_mint(loan_cash_vault_ai)?;
        let loan_cash_feed = find_feed_for_mint(price_feed_ais, &loan_cash_mint)?;
        let elapsed_to_mat = l.maturity_slot.saturating_sub(l.opened_slot);
        let interest = compute_simple_interest(
            l.principal_amount,
            l.interest_bps_per_year,
            elapsed_to_mat,
        )?;
        let total_native = (l.principal_amount as u128)
            .checked_add(interest as u128)
            .ok_or(err::INTEREST_OVERFLOW)?;
        let liab_usd = balance_to_micro_usd_no_haircut(
            total_native,
            loan_cash_feed.price_micro_usd,
            loan_cash_feed.mint_decimals,
        )?;
        existing_liab_usd = existing_liab_usd
            .checked_add(liab_usd)
            .ok_or(err::PRICE_OVERFLOW)?;
    }
    for i in 0..loan_list_entries_count {
        if !covered[i] {
            return Err(err::MARGIN_LOAN_SET_MISMATCH);
        }
    }

    // ── 6. Project the new loan's liability through its maturity,
    //    then convert to micro-USD via the cash mint's feed.
    let new_elapsed_to_mat = new_maturity_slot.saturating_sub(now_slot);
    let new_interest = compute_simple_interest(
        new_principal_amount,
        new_interest_bps_per_year,
        new_elapsed_to_mat,
    )?;
    let new_total_native = (new_principal_amount as u128)
        .checked_add(new_interest as u128)
        .ok_or(err::INTEREST_OVERFLOW)?;
    let new_liab_usd = balance_to_micro_usd_no_haircut(
        new_total_native,
        cash_feed.price_micro_usd,
        cash_feed.mint_decimals,
    )?;

    // ── 7. The gate (in micro-USD).
    let total_liab_usd = existing_liab_usd
        .checked_add(new_liab_usd)
        .ok_or(err::PRICE_OVERFLOW)?;
    if pool_usd < total_liab_usd {
        return Err(err::MARGIN_INSUFFICIENT);
    }
    Ok(())
}

/// Linear-scan a validated PriceFeed slice for the entry whose
/// `mint` field matches. Slice was already PDA-validated; here we
/// only need the mint match. Returns by value (small struct, cheap
/// to copy) so the caller can drop the AccountInfo borrow.
fn find_feed_for_mint(
    feeds: &[AccountInfo],
    mint: &[u8; 32],
) -> Result<PriceFeed, ProgramError> {
    for ai in feeds {
        let data = ai.try_borrow_data()?;
        if data.len() < PriceFeed::LEN {
            continue;
        }
        let pf: &PriceFeed = bytemuck::try_from_bytes(&data[..PriceFeed::LEN])
            .map_err(|_| err::PRICE_FEED_MISSING)?;
        if &pf.mint == mint {
            return Ok(*pf);
        }
    }
    Err(err::PRICE_FEED_MISSING)
}

/// Read a vault account and return its `mint` field.
fn read_vault_mint(vault_ai: &AccountInfo) -> Result<[u8; 32], ProgramError> {
    let data = vault_ai.try_borrow_data()?;
    if data.len() < Vault::LEN {
        return Err(err::MARGIN_POSITION_MISMATCH);
    }
    let v: &Vault = bytemuck::try_from_bytes(&data[..Vault::LEN])
        .map_err(|_| err::MARGIN_POSITION_MISMATCH)?;
    Ok(v.mint)
}

/// Convert a haircut-adjusted balance to micro-USD.
/// `balance × price × credit_bps / (10^decimals × 10_000)`,
/// computed with checked u128 arithmetic.
fn balance_to_micro_usd(
    balance: u64,
    price_micro_usd: u64,
    mint_decimals: u8,
    credit_bps: u128,
) -> Result<u128, ProgramError> {
    let bal_x_price = (balance as u128)
        .checked_mul(price_micro_usd as u128)
        .ok_or(err::PRICE_OVERFLOW)?;
    let decimal_divisor = 10u128
        .checked_pow(mint_decimals as u32)
        .ok_or(err::PRICE_OVERFLOW)?;
    // Divide before the credit_bps multiply to keep the next product
    // bounded. Loses up to (10^decimals − 1) micro-USD per position
    // — at most ~1e-6 USD with 6-decimals tokens, negligible across a
    // realistic portfolio.
    let bal_in_micro_usd = bal_x_price / decimal_divisor;
    let credit_adjusted = bal_in_micro_usd
        .checked_mul(credit_bps)
        .ok_or(err::PRICE_OVERFLOW)?;
    Ok(credit_adjusted / 10_000)
}

/// Convert a u128 native-unit amount to micro-USD without applying
/// a haircut. Used for loan liabilities: principal + interest owed
/// at maturity is "a dollar owed" regardless of asset class.
fn balance_to_micro_usd_no_haircut(
    amount: u128,
    price_micro_usd: u64,
    mint_decimals: u8,
) -> Result<u128, ProgramError> {
    let amt_x_price = amount
        .checked_mul(price_micro_usd as u128)
        .ok_or(err::PRICE_OVERFLOW)?;
    let decimal_divisor = 10u128
        .checked_pow(mint_decimals as u32)
        .ok_or(err::PRICE_OVERFLOW)?;
    Ok(amt_x_price / decimal_divisor)
}

/// Validate a position belongs to the borrower and references the
/// expected vault, then compute its micro-USD contribution to the
/// pool. `seen_positions` detects same-pubkey dupes across the call.
fn add_position_usd(
    position_ai: &AccountInfo,
    vault_ai: &AccountInfo,
    borrower_key: &Pubkey,
    feed: &PriceFeed,
    credit_bps: u128,
    seen_positions: &mut [[u8; 32]],
    seen_count: &mut usize,
) -> Result<u128, ProgramError> {
    let pos_key: &[u8; 32] = position_ai.key();
    for i in 0..*seen_count {
        if &seen_positions[i] == pos_key {
            return Err(err::MARGIN_POSITION_MISMATCH);
        }
    }
    if *seen_count >= seen_positions.len() {
        return Err(err::MARGIN_POSITION_MISMATCH);
    }
    seen_positions[*seen_count] = *pos_key;
    *seen_count += 1;

    let data = position_ai.try_borrow_data()?;
    if data.len() < Position::LEN {
        return Err(err::MARGIN_POSITION_MISMATCH);
    }
    let p: &Position = bytemuck::try_from_bytes(&data[..Position::LEN])
        .map_err(|_| err::MARGIN_POSITION_MISMATCH)?;
    if &p.depositor != borrower_key {
        return Err(err::MARGIN_POSITION_MISMATCH);
    }
    if &p.vault != vault_ai.key() {
        return Err(err::MARGIN_POSITION_MISMATCH);
    }
    balance_to_micro_usd(p.available(), feed.price_micro_usd, feed.mint_decimals, credit_bps)
}

/// Read a vault's `asset_class` field. Decodes the full `Vault`
/// struct since the field's offset isn't part of the public layout
/// guarantees pinned by tests today; a one-time decode keeps the
/// helper future-proof against `Vault` reordering.
fn read_vault_asset_class(vault_ai: &AccountInfo) -> Result<u8, ProgramError> {
    let data = vault_ai.try_borrow_data()?;
    if data.len() < Vault::LEN {
        return Err(err::MARGIN_POSITION_MISMATCH);
    }
    let v: &Vault = bytemuck::try_from_bytes(&data[..Vault::LEN])
        .map_err(|_| err::MARGIN_POSITION_MISMATCH)?;
    Ok(v.asset_class)
}

/// Validate a position belongs to the borrower and references the
/// expected vault, then add its haircut-adjusted `available()` to the
/// pool. `credit_bps` is `10_000 − haircut_bps`. `seen_positions`
/// detects same-pubkey dupes across the whole call (covers the
/// always-passed pair as well as the extras list).
fn add_position_to_pool(
    position_ai: &AccountInfo,
    vault_ai: &AccountInfo,
    borrower_key: &Pubkey,
    credit_bps: u128,
    pool_bps: &mut u128,
    seen_positions: &mut [[u8; 32]],
    seen_count: &mut usize,
) -> ProgramResult {
    let pos_key: &[u8; 32] = position_ai.key();
    for i in 0..*seen_count {
        if &seen_positions[i] == pos_key {
            return Err(err::MARGIN_POSITION_MISMATCH);
        }
    }
    if *seen_count >= seen_positions.len() {
        return Err(err::MARGIN_POSITION_MISMATCH);
    }
    seen_positions[*seen_count] = *pos_key;
    *seen_count += 1;

    let data = position_ai.try_borrow_data()?;
    if data.len() < Position::LEN {
        return Err(err::MARGIN_POSITION_MISMATCH);
    }
    let p: &Position = bytemuck::try_from_bytes(&data[..Position::LEN])
        .map_err(|_| err::MARGIN_POSITION_MISMATCH)?;
    if &p.depositor != borrower_key {
        return Err(err::MARGIN_POSITION_MISMATCH);
    }
    if &p.vault != vault_ai.key() {
        return Err(err::MARGIN_POSITION_MISMATCH);
    }
    let available = p.available() as u128;
    *pool_bps = pool_bps.saturating_add(available.saturating_mul(credit_bps));
    Ok(())
}

/// Remove `loan_pda` from the borrower's `LoanList`. Called from
/// `repay_loan` and `liquidate_loan` once their respective lifecycle
/// checks pass — keeps the list in lockstep with `Loan::status`.
/// Verifies the PDA against the stored `bump` so a swapped-in list
/// can't trick the handler into mutating an arbitrary account.
fn remove_from_loan_list(
    program_id: &Pubkey,
    borrower: &Pubkey,
    loan_list_ai: &AccountInfo,
    loan_pda: &[u8; 32],
) -> ProgramResult {
    let mut data = loan_list_ai.try_borrow_mut_data()?;
    if data.len() < LoanList::LEN {
        return Err(err::LOAN_LIST_LAYOUT_INVALID);
    }
    let ll: &mut LoanList = bytemuck::try_from_bytes_mut(&mut data[..LoanList::LEN])
        .map_err(|_| err::LOAN_LIST_LAYOUT_INVALID)?;
    let expected = create_program_address(
        &[seeds::LOAN_LIST, borrower, &[ll.bump]],
        program_id,
    )
    .map_err(|_| err::LOAN_LIST_PDA_MISMATCH)?;
    if &expected != loan_list_ai.key() {
        return Err(err::LOAN_LIST_PDA_MISMATCH);
    }
    if &ll.borrower != borrower {
        return Err(err::LOAN_LIST_BORROWER_MISMATCH);
    }
    if !ll.remove(loan_pda) {
        return Err(err::LOAN_NOT_IN_LIST);
    }
    ll.last_modified_slot = Clock::get()?.slot;
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

fn cpi_transfer_within_vault(
    vault_program_id: &Pubkey,
    from_depositor_ai: &AccountInfo,
    vault_ai: &AccountInfo,
    from_position_ai: &AccountInfo,
    to_position_ai: &AccountInfo,
    amount: u64,
) -> ProgramResult {
    let mut data = [0u8; 1 + 8];
    data[0] = SSR_VAULT_IX_TRANSFER_WITHIN_VAULT;
    data[1..9].copy_from_slice(&amount.to_le_bytes());

    let metas = [
        AccountMeta::new(from_depositor_ai.key(), false, true),
        AccountMeta::new(vault_ai.key(), false, false),
        AccountMeta::new(from_position_ai.key(), true, false),
        AccountMeta::new(to_position_ai.key(), true, false),
    ];
    let ix = Instruction {
        program_id: vault_program_id,
        data: &data,
        accounts: &metas,
    };
    let infos = [from_depositor_ai, vault_ai, from_position_ai, to_position_ai];
    pinocchio::cpi::slice_invoke_signed(&ix, &infos, &[])
}

fn cpi_seize_locked(
    vault_program_id: &Pubkey,
    authority_ai: &AccountInfo,
    vault_ai: &AccountInfo,
    from_position_ai: &AccountInfo,
    to_position_ai: &AccountInfo,
    amount: u64,
    signer: &Signer<'_, '_>,
) -> ProgramResult {
    let mut data = [0u8; 1 + 8];
    data[0] = SSR_VAULT_IX_SEIZE_LOCKED;
    data[1..9].copy_from_slice(&amount.to_le_bytes());

    let metas = [
        AccountMeta::new(authority_ai.key(), false, true),
        AccountMeta::new(vault_ai.key(), false, false),
        AccountMeta::new(from_position_ai.key(), true, false),
        AccountMeta::new(to_position_ai.key(), true, false),
    ];
    let ix = Instruction {
        program_id: vault_program_id,
        data: &data,
        accounts: &metas,
    };
    let infos = [authority_ai, vault_ai, from_position_ai, to_position_ai];
    pinocchio::cpi::slice_invoke_signed(&ix, &infos, &[signer.clone()])
}

// ─── Test stubs ─────────────────────────────────────────────────────────-

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_in_lending_band() {
        let codes = [
            err::LOAN_PDA_MISMATCH,
            err::NOT_OPEN,
            err::COMPLIANCE_BLOCKED,
            err::MATURED,
            err::NOT_MATURED,
            err::SIGNER_NOT_PARTY,
            err::INTEREST_OVERFLOW,
            err::ZERO_AMOUNT,
        ];
        for e in codes {
            if let ProgramError::Custom(code) = e {
                assert!(
                    (0x5001..=0x5FFF).contains(&code),
                    "code 0x{code:04X} outside lending band"
                );
            }
        }
    }

    #[test]
    fn interest_zero_inputs_return_zero() {
        assert_eq!(compute_simple_interest(0, 500, 1_000_000).unwrap(), 0);
        assert_eq!(compute_simple_interest(1_000_000, 0, 1_000_000).unwrap(), 0);
        assert_eq!(compute_simple_interest(1_000_000, 500, 0).unwrap(), 0);
    }

    #[test]
    fn interest_full_year_matches_bps() {
        // principal 1_000_000 at 500 bps (5%) for SLOTS_PER_YEAR slots
        // = 50_000 interest exactly.
        let interest =
            compute_simple_interest(1_000_000, 500, SLOTS_PER_YEAR as u64).unwrap();
        assert_eq!(interest, 50_000);
    }

    #[test]
    fn interest_half_year_matches_half_bps() {
        let half_year = (SLOTS_PER_YEAR as u64) / 2;
        let interest = compute_simple_interest(1_000_000, 500, half_year).unwrap();
        assert_eq!(interest, 25_000);
    }

    #[test]
    fn interest_overflow_surfaces() {
        // u64::MAX principal * u32::MAX bps * 1 slot definitely
        // exceeds u128.
        let err = compute_simple_interest(u64::MAX, u32::MAX, u64::MAX);
        assert!(matches!(err, Err(ProgramError::Custom(0x5025))));
    }
}
