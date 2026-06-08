# Composition Wrapper Pattern

**Status**: Phase 0e shipped 2026-06-04. Part of the Model C ("hybrid") path described in [`spc-integration.md`](spc-integration.md).
**Applies to**: any SSR program that needs to gate an action on participant compliance — DvP wrappers, margin engine, repo program, lending, vault deposits, etc.

## Why this pattern exists

SSR Phase 0 a/b/c/d shipped compliance enforcement as a **Token-2022 transfer hook**. Every `transfer_checked` against an SSR mint invokes `ssr-compliance`, which reads both participants' `AccountRecord` PDAs and rejects unless both are `VERIFIED`. That model is the strongest cryptographic guarantee — *every* movement of an SSR token is gated.

It also closes some doors:

- SPC's `private-channel-escrow-program` **rejects mints carrying `TransferHook`**. SSR mints using the hook cannot be deposited into a SPC channel.
- Every transfer pays the hook's CU + extra-account-resolution cost — fine for a single trade, painful at scale.
- The compliance decision happens deep inside Token-2022's transfer path, which limits how much context the gate can use (no "this is a DvP settlement" or "this is a margin liquidation" awareness).

The **composition wrapper pattern** is the second supported mode. Compliance enforcement moves *out* of the mint and *into* the program that initiates a meaningful action. The mint stays plain Token-2022. Any SSR composition program (DvP wrapper, margin, repo, ...) imports the check function from `ssr-types` and gates its own instruction.

Both modes coexist. A mint may be issued with `TransferHook` for strict-mode use, with the composition wrapper as a belt-and-braces second check; or without `TransferHook` for SPC-compatibility, with the wrapper as the sole on-chain gate.

## API

`ssr-types` exposes the compliance decision as pure functions (no Pinocchio, no Solana SDK):

```rust
use ssr_types::{
    AccountRecord, CheckError, check_record_bytes, read_account_record, seeds,
};

// One-shot: borrow the buffer and classify.
let result: Result<(), CheckError> = check_record_bytes(account_data);

// Two-shot: parse first, then inspect fields before deciding.
let record: &AccountRecord = read_account_record(account_data)?;
if record.is_accredited() && record.jurisdiction == *b"JP" {
    record.check_transfer_allowed()?;
}
```

`CheckError` discriminates the failure modes:

| Variant | Cause |
|---|---|
| `LayoutInvalid` | buffer too short, mis-aligned, or Pod cast fails |
| `StatusUnknown` | `status` byte outside known discriminant range — fail closed |
| `Unverified` | status is `UNKNOWN` or `PENDING` |
| `Suspended` | status is `SUSPENDED` (temporary hold) |
| `Blocked` | status is `BLOCKED` (terminal under policy) |

For consumers that want the same `ProgramError::Custom` codes the transfer-hook path emits, `ssr_compliance::check_error_to_program_error(CheckError) -> ProgramError` is exposed in the program crate too.

## Wrapper instruction shape

A composition wrapper that needs to verify the parties of a trade follows this skeleton:

```rust
use pinocchio::{
    account::AccountView, address::Address, error::ProgramError, ProgramResult,
};
use ssr_types::{check_record_bytes, seeds};

/// Account ordering for `compliant_*` instructions:
///   [0..N]  the underlying-action accounts (e.g. SPC DvP Swap accounts)
///   [N+0]   source participant's AccountRecord PDA
///   [N+1]   destination participant's AccountRecord PDA
///   ...     more participants as needed
pub fn verify_two_parties(
    source_record: &AccountView,
    dest_record:   &AccountView,
    program_id:    &Address,
    source_owner:  &Address,
    dest_owner:    &Address,
) -> ProgramResult {
    verify_record(source_record, program_id, source_owner)?;
    verify_record(dest_record,   program_id, dest_owner)?;
    Ok(())
}

fn verify_record(
    record_account: &AccountView,
    program_id: &Address,
    expected_owner: &Address,
) -> ProgramResult {
    // 1. Account must be owned by ssr-compliance (or whatever program
    //    you've decided to trust as the registry source of truth).
    if record_account.owner() != ssr_compliance::ID {
        return Err(/* WrongRegistryOwner */);
    }
    // 2. Read the record and confirm it really is the participant we
    //    were told it is. Reject pass-through-an-arbitrary-PDA attacks.
    let data = record_account.try_borrow()?;
    let record = ssr_types::read_account_record(&data)
        .map_err(/* map to your error namespace */)?;
    if &record.participant != expected_owner {
        return Err(/* RecordOwnerMismatch */);
    }
    // 3. Optional: re-derive the PDA from seeds and assert match.
    //    Slower (one hash) but defends against an attacker substituting
    //    a record owned by ssr-compliance but for a different
    //    participant. Skip if you trust the record_owner check + the
    //    fact that ssr-compliance only writes records at the canonical PDA.
    let (expected_pda, _) = pinocchio::address::find_program_address(
        &[seeds::ACCOUNT_RECORD, expected_owner.as_ref()],
        &ssr_compliance::ID,
    );
    if record_account.address() != &expected_pda {
        return Err(/* RecordPdaMismatch */);
    }
    // 4. Now do the compliance decision itself.
    record.check_transfer_allowed()
        .map_err(/* map to your error namespace */)?;
    Ok(())
}
```

Key invariants the wrapper MUST check (in roughly increasing order of paranoia):

1. **The record account is owned by `ssr-compliance`.** Otherwise an attacker could synthesize a fake "VERIFIED" record under their own program and pass it in.
2. **The record's `participant` field matches the participant pubkey the wrapper was told to gate on.** Otherwise an attacker substitutes a record belonging to *some other* verified user.
3. **(Optional, defense in depth)** The PDA derivation matches. Belt-and-braces.

Only (1) and (2) are strictly required; (3) is a `find_program_address` call (~1500 CU) that may be skipped in hot paths.

## When to use each mode

| Situation | Mode |
|---|---|
| Every Token-2022 transfer must be gated, period | TransferHook on the mint |
| Mint will be deposited into a SPC channel | Composition wrapper only (mint cannot have TransferHook) |
| High-throughput retail transfers within a single issuer | TransferHook OK; consider routing routine flows through an SPC channel instead |
| Cross-asset DvP / repo / margin via SPC DvP Swap or our own settlement | Composition wrapper at the SSR wrapper layer; mint can be either |
| Maximum-paranoia / regulator-mandated continuous gating | TransferHook AND composition wrapper both — belt + braces |
| Off-chain admin SDK pre-flight check | The same `check_record_bytes` function compiles cleanly against host-side Rust (it is pinocchio-free) |

## Integration example: SSR DvP wrapper over SPC `dvp-swap-program`

The Phase 1 design is a thin SSR program whose only job is to verify compliance on both parties before invoking SPC's `SettleDvp` via CPI. The wrapper becomes the `settlement_authority` of every DvP it creates.

```
                              ssr-dvp-wrapper                 SPC dvp-swap-program
   client                    (settlement_authority)
     │                                │                               │
     │ create + fund both legs ──────►│                               │
     │   (calls SPC CreateDvp;        │ ───CPI: CreateDvp───────────► │ writes SwapDvp PDA
     │    no compliance checks yet)   │                               │
     │                                │                               │
     │ settle ─────────────────────►│                                │
     │                                │ verify_two_parties(           │
     │                                │   source_record, dest_record) │
     │                                │   ↳ ssr_types::check_record_bytes│
     │                                │                               │
     │                                │ ───CPI: SettleDvp───────────► │ atomically transfers
     │                                │   (signs as authority)        │ both legs, closes PDAs
     │                                │                               │
     │ ◄─────────────────────────────│ ◄─────────────────────────────┤
```

A single SSR program instruction:

```rust
pub fn process_compliant_settle_dvp(
    program_id: &Address,
    accounts:   &[AccountView],
    data:       &[u8],
) -> ProgramResult {
    // Layout:
    //   [0..N]   accounts forwarded to SPC SettleDvp (the dvp PDA, the
    //            mints, escrow ATAs, user ATAs, token programs, +
    //            transfer-hook extras split by leg_a_extras_count)
    //   [N]      source participant's AccountRecord PDA
    //   [N+1]    destination participant's AccountRecord PDA
    let (forward, gate) = split_accounts(accounts);
    let [source_record, dest_record] = gate else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    verify_two_parties(
        source_record,
        dest_record,
        program_id,
        &derive_user_a(forward)?, // pull user_a out of the SwapDvp PDA data
        &derive_user_b(forward)?,
    )?;

    cpi_into_spc_settle_dvp(forward, data)
}
```

Lines of program code: ~150 (including account-ordering helpers). The actual atomic-swap mechanics, expiry handling, surplus refund, TransferHook extras forwarding — all of that is SPC's. SSR contributes the compliance gate.

## Off-chain SDK usage

Because `ssr-types` is pinocchio-free, the same check function compiles against host-side Rust. Admin tooling, monitoring dashboards, and operator CLIs can run the same decision locally before sending the on-chain tx:

```rust
// In an admin CLI written against solana-sdk:
let acc = rpc_client.get_account(&account_record_pda)?;
match ssr_types::check_record_bytes(&acc.data) {
    Ok(()) => println!("✓ verified"),
    Err(ssr_types::CheckError::Suspended) => println!("⏸ suspended"),
    Err(ssr_types::CheckError::Blocked) => println!("⛔ blocked"),
    Err(e) => println!("✗ {e:?}"),
}
```

This is the same code path the on-chain wrapper takes. Drift between off-chain pre-flight and on-chain enforcement is impossible by construction.

## Testing

`ssr-types`' check API is unit-tested in `crates/ssr-types/src/lib.rs::tests`. Composition wrappers should:

1. Reuse the existing test vectors against their wrapper-translated error types.
2. Add LiteSVM E2E tests demonstrating the wrapper rejects bad records (suspended / blocked / wrong-participant) and accepts good ones.
3. Include at least one E2E test that exercises the full composition: SSR wrapper instruction → SPC primitive CPI → Token-2022 transfer landing.

See `programs/ssr-compliance/tests/e2e_token.rs` for the testing harness pattern.
