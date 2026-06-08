# SPC vs SSR — 1-page position memo

**Date**: 2026-06-04
**Use**: 1-page positioning between Solana Foundation's SPC and SSR's prime-broker layer. Full integration analysis in [`spc-integration.md`](spc-integration.md).

## Headline

**SPC (Solana Private Channels) is not a replacement for SSR. The two are different layers. SSR's Phase 1 (DvP) currently uses SPC's `dvp-swap-program` as the atomic settlement primitive — that is the cleanest composition today.**

## What SPC is

**Solana Foundation's institutional payment-channel reference (unaudited at the time of this memo)**. Three separable pieces:

1. **Channel (off-chain)** — bank-operated high-throughput private settlement rail.
2. **Escrow program (mainnet)** — token in/out for the channel. **Rejects mints carrying TransferHook.**
3. **DvP Swap program (mainnet)** — 2-party atomic exchange. **Full TransferHook support**, Pinocchio + no_std.

(1)+(2) is a channel-centric single-issuer model. (3) is a standalone mainnet primitive.

## Five structural differences vs SSR

| Axis | SSR | SPC channel |
|---|---|---|
| **Trust model** | Math (compliance gate is on-chain cryptography) | Bank operator (compliance gate is off-chain settler) |
| **Composability** | Cross-issuer / cross-asset is natural | Single-issuer self-contained; cross-bank requires mainnet |
| **Throughput** | Mainnet-bound | High within the channel |
| **Privacy** | Public on-chain | Channel-internal private |
| **Operational load** | Program deploy only | Full stack (PG + Redis + indexer + operator + auth + gateway) |

## Adoption guidance (one line per decision)

- **Cross-issuer / cross-asset prime broker** → **SSR** (the channel model cannot structurally solve this).
- **Single-bank, high-frequency customer payments, privacy required** → **SPC channel** (Foundation's intended use case).
- **Multi-issuer + atomic DvP** → **SSR mint + SPC DvP Swap program** (composed).
- **Production deploy today** → **SSR only** (SPC remains unaudited).
- **Single-bank high-UX retail demo** → **SPC channel** (UX advantage).

## Three operating models

| Model | Contents | Applicability |
|---|---|---|
| **A. SSR-only** | Token-2022 + TransferHook → ssr-compliance | Cross-issuer required / maximum mathematical trust |
| **B. SPC-only** | TransferHook-less mint + channel-internal compliance | Single-bank high-throughput / privacy / bank operator trust assumed |
| **C. Hybrid** | TransferHook-less mint + SSR composition wrappers + SPC DvP Swap mainnet primitive | **Recommended default**; covers cross-issuer DvP and lets the channel UX still be used |

## When to recommend SPC, when not

**Recommend**:
- Customer values "Foundation-official" provenance.
- Single-bank high-frequency payments are the primary use case.
- Operational and technical capacity for channel operation is present.
- Privacy is a hard requirement.

**Decline (or caveat)**:
- **Unaudited status → production / real customer-funds deployment is premature.**
- Cross-bank prime broker capability is the primary requirement.
- Operational headcount / infra stack (PG + Redis + indexer etc.) cannot be sustained.
- The deployment must minimize external Solana-ecosystem dependencies.

## Effect on SSR Phase 1 roadmap

- **Original plan**: build `ssr-escrow` from scratch (2-leg atomic swap + expiry + TransferHook integration).
- **Revised plan**: use SPC's `dvp-swap-program`; SSR ships only `ssr-dvp-wrapper` (compliance verify → CPI to SPC DvP).
- **Engineering saved**: ~3 weeks.

## One-line summary

> **SSR is the institutional RWA prime broker layer; SPC is a single-issuer settlement rail. Phase 1 reuses SPC's DvP Swap primitive; from Phase 1 onward the hybrid Model C is the default recommendation. Whether to additionally adopt the SPC channel is a per-use-case decision, and SPC's unaudited status precludes production deployment today.**
