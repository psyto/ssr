# SSR + Solana Private Channels (SPC) Integration

**Date**: 2026-06-04
**Status**: research memo

## 1. Solana Private Channels overview

SPC is the Solana Foundation's official institutional payment-channel reference, released as `solana-foundation/solana-private-channels` (MIT, **not yet audited**). It is a **payment channel** sitting alongside Solana mainnet, optimized for high-throughput permissioned bank-customer payments.

### 1.1 Architecture in one diagram

```
              ┌──────────────────────────────────────────────────┐
              │              SOLANA MAINNET                       │
              │                                                   │
              │   Escrow Program          DvP Swap Program        │
              │   (per-instance,          (P2P atomic swap        │
              │    SMT-based withdraw)     w/ TransferHook)       │
              │      ▲                       ▲                    │
              │      │ deposit              │ settle              │
              │      │ release              │                     │
              │      │   ▲                                        │
              └──────┼───┼───▲────────────────────────────────────┘
                     │   │   │
                     │   │   │ indexer events / operator txs
                     │   │   │
              ┌──────┼───┼───┼────────────────────────────────────┐
              │      │   │   │       SPC OFF-CHAIN STACK          │
              │      │   │   │                                    │
              │   Indexer (gRPC/RPC) ───► PostgreSQL              │
              │      │                       ▲                     │
              │   Operator ──────────────────┤                     │
              │      ▼                       │                     │
              │   ┌──────────────────────┐   │                     │
              │   │ SPC Core (channel)   │   │                     │
              │   │ ┌──────────────────┐ │   │                     │
              │   │ │ 5-stage pipeline │ │   │                     │
              │   │ │ Dedup→SigV→Seq→ │ │   │                     │
              │   │ │ Exec→Settle      │ │───┘                     │
              │   │ └──────────────────┘ │  (100ms batches)        │
              │   └──────────────────────┘                         │
              │      ▲                                              │
              │   Gateway (JSON-RPC + optional Auth/JWT)            │
              │      ▲                                              │
              └──────┼──────────────────────────────────────────────┘
                     │
                  Customers (private RPC)
```

### 1.2 What SPC provides

| Component | Mainnet / Off-chain | Purpose |
|---|---|---|
| **Escrow Program** | mainnet | per-instance token custody; admin owns mint allowlist + operator list; SMT-based double-spend protection on `ReleaseFunds` |
| **Withdraw Program** | channel | burns channel tokens, emits event → indexer triggers mainnet `ReleaseFunds` |
| **DvP Swap Program** | mainnet | 2-party atomic asset↔cash swap; settlement_authority signs; **TransferHook extras forwarded** to both legs |
| **Core** | off-chain | 5-stage SVM-execution pipeline (Dedup/SigVerify/Sequencer/Executor/Settler); batches every 100ms to PostgreSQL |
| **AdminVM** | off-chain | privileged mint operations only (`InitializeMint` bypasses BPF) |
| **GaslessCallback** | off-chain | synthesizes fee-payer accounts → zero gas for users |
| **Indexer** | off-chain | mainnet/channel monitoring; Yellowstone gRPC or RPC polling |
| **Operator** | off-chain | turns indexed events into mainnet tx (release funds, mint in channel) |
| **Auth** | off-chain | optional JWT + wallet auth, RBAC at gateway |

### 1.3 Channel-internal limitations (read carefully)

| Constraint | Implication |
|---|---|
| **Only SPL Token / ATA / Memo / System / Withdraw programs run inside the channel** | SSR `ssr-compliance` program **CANNOT** run inside the channel. Compliance must be enforced via the bank's off-chain Executor/Settler stages OR via mainnet-side gates only. |
| **AdminVM only intercepts `InitializeMint`** | Mint authority operations are admin-only; arbitrary program logic in admin context unsupported. |
| **No custom BPF deploy** | Channel is a curated SVM execution environment, not a permissionless chain. |
| **No precompiles** | Non-Ed25519 signature schemes (Secp256r1 for institutional HSM) cannot be verified on-chain inside the channel. |
| **No fork choice / no rollback** | Finality on write; all blocks final once persisted. |

## 2. The TransferHook compatibility problem

### 2.1 SPC Escrow REJECTS Token-2022 mints carrying `TransferHook`

In `private-channel-escrow-program/program/src/processor/shared/token_utils.rs`:

```rust
pub fn validate_token2022_extensions(mint_info: &AccountView) -> ProgramResult {
    let data = mint_info.try_borrow()?;
    let mint = StateWithExtensions::<Token2022MintState>::unpack(&data)?;
    if mint.get_extension::<TransferHook>().is_ok() {
        return Err(PrivateChannelEscrowProgramError::TransferHookNotAllowed.into());
    }
    Ok(())
}
```

This check fires on **`Deposit`**, **`ReleaseFunds`**, and **`AllowMint`**. Practical consequence:

> An SSR-style mint that uses `TransferHook` → `ssr-compliance` for on-chain compliance gating cannot be deposited into the SPC Escrow. The Escrow Program path is closed for it.

### 2.2 SPC DvP Swap Program ACCEPTS `TransferHook`

In `dvp-swap-program/program/src/processor/settle_dvp.rs`:

> "Trailing accounts (variable): First `leg_a_extras_count` go to leg A's `TransferChecked` CPI (hook program, validation PDA, and any accounts resolved from `ExtraAccountMetaList`). The rest go to leg B's `TransferChecked` CPI."

The DvP Swap Program is **TransferHook-aware by design** and forwards the extras correctly. SSR's compliance gate fires automatically on both legs of an atomic swap.

This is the cleanest possible composition: **SSR Token-2022 mints ⊕ SPC DvP Swap Program = atomic permissioned DvP with compliance enforced at the on-chain layer**.

### 2.3 Where the two stances come from

- **SPC Escrow** assumes the channel itself is the permission boundary; admitting an external on-chain hook would create unbounded compute / failure modes during deposit/release. Compliance is the operator's job, off-chain.
- **SPC DvP Swap** sits as a *mainnet primitive* outside the channel model; it has to interoperate with whatever mints the broader Solana ecosystem produces, including TransferHook-gated ones.

The two SPC programs are **independent**. Adopting one does not commit you to the other.

## 3. Three operating models for institutional RWA on Solana

| Model | Mint type | Compliance enforcement | Throughput | Privacy | When to choose |
|---|---|---|---|---|---|
| **A. SSR-only (current)** | Token-2022 + TransferHook → `ssr-compliance` | on-chain, every transfer, cryptographic | mainnet-bound (~3000 TPS theoretical, much less practical) | public | cross-issuer cross-asset prime broker; no operator trust assumption |
| **B. SPC-only** | Token-2022 without `TransferHook` (compliance off-chain in channel) | off-chain, channel-internal, operator-enforced | high (channel batches) | private (channel-internal) | single-issuer high-throughput payments; operator trust is institutional norm |
| **C. Hybrid** | Token-2022 **without** TransferHook on the mint itself; compliance enforced by **wrapping the DvP/transfer instruction** via SSR compliance program | on-chain at composition points (DvP / repo / margin), off-chain inside SPC channel for routine transfers | mixed — channel for hot path, mainnet for compositions | mixed | bank wants channel UX for customers + cross-issuer composability at the prime broker layer |

### 3.1 Model A — SSR-only

Already built (Phase 0 complete). Every transfer goes through the on-chain gate. Strength: maximum cryptographic guarantee, multi-issuer composability, no trusted operator. Weakness: every transfer hits mainnet → cost + latency floor.

### 3.2 Model B — SPC-only

Bank A creates a Token-2022 mint **without TransferHook**, configures SPC Escrow to allow it, runs an SPC channel for their customers. Internal payments are private, near-instant, and zero-fee. Withdrawals exit via Burn-in-channel → SMT-proven Release-on-mainnet.

Compliance: enforced by the bank's Settler-stage business logic in the channel + the channel's RPC allowlist.

**Cross-issuer interop**: limited. Bank A's channel can't see Bank B's channel. A cross-bank DvP requires both to settle on mainnet (defeating the channel's purpose for that transaction).

### 3.3 Model C — Hybrid (recommended for SSR)

```
                ┌──────────────────────────────────────────────────┐
                │                MAINNET                            │
                │                                                   │
                │  Token-2022 mints (no TransferHook on the mint)   │
                │  • Bank A deposit token                            │
                │  • Bank B sovereign-bond ST                        │
                │  • Issuer C real-asset ST                          │
                │                                                   │
                │  ssr-compliance program (registry of allowed      │
                │  participants; PDA per participant)               │
                │                                                   │
                │  ssr-margin / ssr-repo / ssr-dvp wrappers         │
                │  call ssr-compliance to verify both parties        │
                │  before calling Token-2022 transfer or the         │
                │  SPC DvP Swap Program.                             │
                │                                                   │
                │  SPC DvP Swap Program (mainnet primitive)         │
                └──────────────────────────────────────────────────┘
                         ▲                            ▲
                         │ deposit/release (allowed)  │ used as the
                         │                            │ atomic settle
                         │                            │ point
                ┌────────┼────────────────────────────┼──────────────┐
                │  SPC channel(s) per issuer (optional)              │
                │   (bank uses for internal high-throughput payments) │
                │   compliance runs in the channel Settler stage      │
                └─────────────────────────────────────────────────────┘
```

Key moves:

- **Drop the on-mint TransferHook.** Don't gate every Token-2022 transfer cryptographically. Instead, the mint authority + the SSR composition programs gate every *meaningful* movement.
- **Compliance becomes a callable program.** `ssr-compliance::check(participant, asset_class)` returns Ok/Err. The DvP wrapper, margin program, repo program, etc. call this before issuing the actual transfer CPI.
- **Routine transfers go through SPC channels.** Channel Settler stage enforces the same compliance check off-chain (read from a synced copy of the on-chain registry).
- **Composition transfers (DvP, repo, lending) settle on mainnet** using the SPC DvP Swap Program (or our own wrapper) which checks compliance + executes atomically.

Trade-offs:

| Pro | Con |
|---|---|
| Cross-issuer DvP works (mainnet primitive) | Compliance has two enforcement points (channel + mainnet); both must stay in sync |
| Channel UX (instant, private, gasless) available where it matters | The on-mint cryptographic guarantee is weaker — relies on integration discipline |
| SPC Escrow can hold SSR-eligible mints (because no TransferHook) | Per-issuer channel is still single-operator-trust; cross-bank DvP must hit mainnet |
| Foundation-sanctioned channel infrastructure ⇒ easier institutional procurement | Higher integration complexity than either A or B alone |

## 4. What this means for Phase 1 (DvP)

**Original plan**: build `ssr-escrow` from scratch — 2-leg atomic swap, expiry/cancel, TransferHook integration, etc.

**Revised plan**: **adopt SPC `dvp-swap-program` as the atomic settlement primitive**. It already provides:

- 2-party DvP (user_a, user_b, mint_a, mint_b)
- `settlement_authority` as a designated atomic settler (perfect for SSR's compliance-checked path)
- Expiry + earliest_settlement timestamps
- Reclaim / Cancel / Reject lifecycle
- **TransferHook extras correctly split between legs** (`leg_a_extras_count: u8`)
- Pinocchio + Token-2022 + no_std (same stack as SSR)

What SSR adds on top:

1. **`ssr-dvp-wrapper` program**: a thin program whose only job is to verify compliance on both parties before CPI-ing into SPC's `SettleDvp`. Becomes the `settlement_authority` of every DvP. Single instruction: `compliance_settle_dvp(dvp_pda, ...)`.
2. **Phase 0 compliance program stays as-is** for any mint that does keep `TransferHook` (legacy/strict-mode); but Phase 1 DvP wrappers do not require it.
3. **Eliminate `ssr-escrow` from the Phase 1 scope.** Saved ~3 weeks of engineering.

This reframes the Phase 1 ship cost as: thin compliance wrapper + integration tests + LiteSVM e2e demo with SPC's DvP Swap Program.
