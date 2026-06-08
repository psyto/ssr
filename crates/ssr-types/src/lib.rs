//! `ssr-types` — shared on-chain primitives for the SSR platform.
//!
//! All types in this crate are `bytemuck::Pod`-compatible so programs can
//! borrow account data as `&AccountRecord` directly, with no allocation
//! and no copy. The crate is `no_std` so it links cleanly into BPF.
//!
//! The vocabulary is intentionally minimal at Phase 0 — only what the
//! compliance registry needs. Token / vault / margin types will be added
//! in their own modules as their programs land.

#![no_std]

use bytemuck::{Pod, Zeroable};

// ─── Compliance status discriminants ─────────────────────────────────────
//
// Stored as `u8` inside `AccountRecord`. Constants (rather than a Rust
// `enum`) keep the type `Pod`-derivable; programs validate the value range
// at deserialization sites.

/// Compliance status values held in `AccountRecord::status`.
pub mod compliance_status {
    /// Slot is zero / never registered. Default for fresh accounts.
    pub const UNKNOWN: u8 = 0;
    /// Registration in flight, awaiting onboarding review.
    pub const PENDING: u8 = 1;
    /// Cleared for permitted transfers per the participant's tier.
    pub const VERIFIED: u8 = 2;
    /// Temporarily suspended (e.g., during sanctions screening).
    pub const SUSPENDED: u8 = 3;
    /// Permanently blocked.
    pub const BLOCKED: u8 = 4;

    /// True when the status is a recognized value (not a forward-compat
    /// reserved discriminant). Programs should reject unknown discriminants
    /// at deserialization rather than silently mis-interpret them.
    #[must_use]
    pub const fn is_known(status: u8) -> bool {
        matches!(status, UNKNOWN | PENDING | VERIFIED | SUSPENDED | BLOCKED)
    }
}

// ─── Jurisdiction codes ──────────────────────────────────────────────────
//
// ISO 3166-1 alpha-2, packed as `[u8; 2]` so they fit naturally next to
// other small fields in `AccountRecord`.

/// 2-byte ISO 3166-1 alpha-2 country codes for the jurisdictions SSR
/// currently has an onboarding flow for. Add as needed.
pub mod jurisdiction {
    pub const NONE: [u8; 2] = [0, 0];
    pub const JP: [u8; 2] = *b"JP";
    pub const US: [u8; 2] = *b"US";
    pub const GB: [u8; 2] = *b"GB";
    pub const SG: [u8; 2] = *b"SG";
    pub const HK: [u8; 2] = *b"HK";
    pub const KY: [u8; 2] = *b"KY"; // Cayman — common fund jurisdiction
}

// ─── Asset class discriminants ───────────────────────────────────────────
//
// Used by issuance / collateral programs. Lives here so every program sees
// the same tag space; introducing a new asset class is a single edit.

/// Asset class tags assigned to mints at issuance time.
pub mod asset_class {
    pub const UNKNOWN: u8 = 0;
    /// 預金トークン — bank-issued cash-equivalent.
    pub const TOKENIZED_DEPOSIT: u8 = 1;
    /// Stablecoin (regulated SC, not deposit-token).
    pub const STABLECOIN: u8 = 2;
    /// Sovereign bond ST (国債).
    pub const SOVEREIGN_BOND: u8 = 3;
    /// Corporate bond ST (社債).
    pub const CORPORATE_BOND: u8 = 4;
    /// Equity ST (株式).
    pub const EQUITY: u8 = 5;
    /// Fund unit / MMF / 投信.
    pub const FUND_UNIT: u8 = 6;
    /// Real asset (real estate / infrastructure / energy).
    pub const REAL_ASSET: u8 = 7;
    /// Commodity.
    pub const COMMODITY: u8 = 8;
}

// ─── Collateral haircut table ───────────────────────────────────────────-
//
// Per-asset-class collateral haircut, expressed in basis points (0–10_000).
// A haircut of 3_000 means a position contributes 70% of its nominal value
// to cross-margin credit. UNKNOWN / unrecognized classes haircut at 100%
// (zero credit) so a misconfigured vault never silently inflates equity.
//
// Two consumers:
//   * `ssr-cli margin show` — when a `RiskParams` PDA is allocated, the
//     CLI reads its `haircut_bps` table. When the PDA is absent (pre-v1c
//     deployments, lending-only demos), the CLI falls back to
//     `default_haircut_bps` below.
//   * `ssr-compliance::initialize_risk_params` — seeds the PDA's
//     `haircut_bps` array from `DEFAULT_HAIRCUTS` so a fresh deployment
//     boots with sensible defaults that match the pre-v1c hardcoded table.
//
// The figures are indicative, not policy: tune before any production use.

/// Default haircut for each known `asset_class` discriminant, indexed by
/// the discriminant value. Used to seed a fresh `RiskParams` PDA and as
/// the CLI fallback when the PDA isn't allocated. Indices above the
/// known range read as 10_000 (full haircut) so a future asset_class
/// addition fails closed until governance updates the table.
pub const DEFAULT_HAIRCUTS: [u16; RiskParams::HAIRCUT_TABLE_LEN] = {
    let mut t = [10_000u16; RiskParams::HAIRCUT_TABLE_LEN];
    t[asset_class::TOKENIZED_DEPOSIT as usize] = 0;
    t[asset_class::STABLECOIN as usize]        = 0;
    t[asset_class::SOVEREIGN_BOND as usize]    = 500;
    t[asset_class::CORPORATE_BOND as usize]    = 1_500;
    t[asset_class::FUND_UNIT as usize]         = 2_000;
    t[asset_class::COMMODITY as usize]         = 2_500;
    t[asset_class::EQUITY as usize]            = 3_000;
    t[asset_class::REAL_ASSET as usize]        = 4_000;
    t
};

/// CLI-side fallback when no `RiskParams` PDA is allocated. Indexes
/// `DEFAULT_HAIRCUTS` with a bounds check that returns 10_000 (full
/// haircut, no credit) for unknown discriminants — same fail-closed
/// shape as the on-chain `RiskParams::haircut_for` reader.
#[must_use]
pub fn default_haircut_bps(class: u8) -> u16 {
    let idx = class as usize;
    if idx < RiskParams::HAIRCUT_TABLE_LEN {
        DEFAULT_HAIRCUTS[idx]
    } else {
        10_000
    }
}

/// Legacy const-fn entry point retained for callers (`ssr-cli`,
/// integration tests) that haven't migrated to either reading
/// `RiskParams` or calling `default_haircut_bps` directly.
#[must_use]
#[inline]
pub fn haircut_bps(class: u8) -> u16 {
    default_haircut_bps(class)
}

/// Display label for an `asset_class` discriminant — used by CLI
/// pretty-printers and operator-facing logs.
#[must_use]
pub fn asset_class_label(class: u8) -> &'static str {
    match class {
        asset_class::UNKNOWN           => "UNKNOWN",
        asset_class::TOKENIZED_DEPOSIT => "TOKENIZED_DEPOSIT",
        asset_class::STABLECOIN        => "STABLECOIN",
        asset_class::SOVEREIGN_BOND    => "SOVEREIGN_BOND",
        asset_class::CORPORATE_BOND    => "CORPORATE_BOND",
        asset_class::EQUITY            => "EQUITY",
        asset_class::FUND_UNIT         => "FUND_UNIT",
        asset_class::REAL_ASSET        => "REAL_ASSET",
        asset_class::COMMODITY         => "COMMODITY",
        _ => "<unrecognized>",
    }
}

// ─── AccountRecord flags ─────────────────────────────────────────────────
//
// A single `u8` carries qualifier bits independent of the headline status.
// Most use cases need only the `ACCREDITED` bit at Phase 0; the others are
// reserved so we don't have to migrate account layout when they arrive.

/// Bit flags in `AccountRecord::flags`.
pub mod flags {
    /// Participant is an accredited / qualified institutional investor.
    pub const ACCREDITED: u8 = 0b0000_0001;
    /// Participant is a professional / pro-equivalent counterparty.
    pub const PROFESSIONAL: u8 = 0b0000_0010;
    /// Participant is itself a regulated institution (bank / broker / etc.).
    pub const REGULATED_ENTITY: u8 = 0b0000_0100;
}

// ─── AccountRecord ───────────────────────────────────────────────────────

/// Per-participant compliance record, stored in a PDA keyed on the
/// participant's wallet pubkey. The compliance program is the only writer;
/// every other SSR program reads this struct to gate transfers, deposits,
/// borrows, etc.
///
/// Layout is fixed at 56 bytes. Field ordering puts `u64` first so the
/// struct is naturally 8-byte aligned with zero internal padding — this
/// is what lets `bytemuck::Pod` derive cleanly. Future expansions go into
/// `_reserved`; never repurpose existing fields without a versioned
/// migration path.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct AccountRecord {
    /// Slot at which `status` last changed. Programs use this to enforce
    /// re-screening cadence; comparison is against `Clock::slot`.
    /// Placed first to satisfy the struct's 8-byte alignment requirement
    /// without inserting internal padding.
    pub updated_at_slot: u64,
    /// Owning participant's Solana pubkey.
    pub participant: [u8; 32],
    /// `compliance_status::*`.
    pub status: u8,
    /// `jurisdiction::*` 2-byte code.
    pub jurisdiction: [u8; 2],
    /// `flags::*` bit set.
    pub flags: u8,
    /// PDA bump seed cached at creation so the hot read path doesn't have
    /// to call `find_program_address` (one syscall + ~1500 CU saved).
    pub bump: u8,
    /// Reserved for forward-compatible additions (sub-tier, screening
    /// cohort id, etc.). Zero on initialization.
    pub _reserved: [u8; 11],
}

impl AccountRecord {
    /// Serialized byte length (= `size_of::<Self>()`). PDAs are sized to
    /// exactly this on `initialize`.
    pub const LEN: usize = core::mem::size_of::<Self>();

    /// Construct a freshly-onboarded record in `PENDING` status. Programs
    /// should write this via `bytemuck::write` (or equivalent) into the
    /// PDA's data buffer.
    #[must_use]
    pub fn pending(participant: [u8; 32], jurisdiction: [u8; 2], slot: u64, bump: u8) -> Self {
        Self {
            updated_at_slot: slot,
            participant,
            status: compliance_status::PENDING,
            jurisdiction,
            flags: 0,
            bump,
            _reserved: [0; 11],
        }
    }

    /// True if `status == VERIFIED`. Other programs use this as the
    /// transfer-permission predicate.
    #[must_use]
    pub const fn is_verified(&self) -> bool {
        self.status == compliance_status::VERIFIED
    }

    /// True if the accredited-investor flag is set.
    #[must_use]
    pub const fn is_accredited(&self) -> bool {
        (self.flags & flags::ACCREDITED) != 0
    }
}

// ─── Registry ────────────────────────────────────────────────────────────
//
// Global admin authority record. One per deployment. Stored at a PDA
// derived from `seeds::REGISTRY` so callers (and other SSR programs) can
// rediscover it deterministically.
//
// The A4 hybrid authority model: `super_admin` is a multisig-style root
// authority (typically a Squads multisig) that can rotate the
// operational roles. The operational roles (`onboard_operator`,
// `status_operator`) are single signers held by the compliance ops team
// for routine, low-blast-radius actions.

/// Operational role discriminants.
pub mod role {
    pub const ONBOARD: u8 = 0;
    pub const STATUS: u8 = 1;
    /// Phase 4 v1d — oracle operator. Signs `update_price` to refresh
    /// per-mint `PriceFeed` PDAs. Separate from `super_admin` so the
    /// price-feed cadence (often automated) doesn't share keys with
    /// the multisig that gates RiskParams + role rotations.
    pub const ORACLE: u8 = 2;

    #[must_use]
    pub const fn is_known(role: u8) -> bool {
        matches!(role, ONBOARD | STATUS | ORACLE)
    }
}

/// Global registry account. Holds the admin authority pubkeys.
///
/// Layout fixed at 144 bytes. The `bump` byte is cached so transfer-hook
/// or rotate flows don't have to re-derive the registry PDA.
///
/// Phase 4 v1d migration note: `oracle_operator` lives at offset 112
/// (re-purposed from the 32-byte `_reserved` slack that used to end the
/// struct). Pre-v1d Registry PDAs naturally read as
/// `oracle_operator == [0; 32]` — i.e., no oracle authority set —
/// which fails `update_price` closed until super-admin rotates the
/// role. Total size unchanged at 144 bytes; the layout pin still holds.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Registry {
    /// Slot at which the registry was last mutated.
    pub last_modified_slot: u64,
    /// Multisig (or single) root authority. Can rotate operational roles
    /// and execute migrations.
    pub super_admin: [u8; 32],
    /// Single-signer authority for `register_account`.
    pub onboard_operator: [u8; 32],
    /// Single-signer authority for `update_status`.
    pub status_operator: [u8; 32],
    /// Layout version. Bump on any field-shape migration.
    pub version: u16,
    /// PDA bump.
    pub bump: u8,
    /// Pad to next 8-byte boundary so `oracle_operator` is aligned.
    pub _pad: [u8; 5],
    /// Single-signer authority for `update_price` on `PriceFeed` PDAs
    /// (Phase 4 v1d). Re-purposed from the original 32-byte
    /// `_reserved` slack; defaults to `[0; 32]` on pre-v1d PDAs,
    /// which fails update_price closed until super-admin rotates it
    /// via `rotate_operators role=oracle`.
    pub oracle_operator: [u8; 32],
}

impl Registry {
    pub const LEN: usize = core::mem::size_of::<Self>();
    pub const CURRENT_VERSION: u16 = 2;

    /// Construct an initial registry. Operational roles default to the
    /// same pubkey as `super_admin` until rotated.
    #[must_use]
    pub fn initial(super_admin: [u8; 32], slot: u64, bump: u8) -> Self {
        Self {
            last_modified_slot: slot,
            super_admin,
            onboard_operator: super_admin,
            status_operator: super_admin,
            version: Self::CURRENT_VERSION,
            bump,
            _pad: [0; 5],
            oracle_operator: super_admin,
        }
    }

    /// Lookup the operator pubkey assigned to a given role.
    #[must_use]
    pub const fn operator_for(&self, r: u8) -> Option<&[u8; 32]> {
        match r {
            role::ONBOARD => Some(&self.onboard_operator),
            role::STATUS => Some(&self.status_operator),
            role::ORACLE => Some(&self.oracle_operator),
            _ => None,
        }
    }
}

// ─── RiskParams (Phase 4 v1c: governance-mutable haircut table) ─────────-
//
// Single global PDA at `seeds::RISK_PARAMS`, owned by `ssr-compliance`.
// Holds the per-asset-class collateral haircut table that `ssr-cli
// margin show` reads (and that v1b's on-chain margin enforcement will
// CPI-check). Governance: super-admin signs both `initialize_risk_params`
// and `set_haircut` — the super_admin pubkey lives in `Registry` and is
// read fresh on every write, so a rotation in `Registry` automatically
// applies here too (no copy to keep in sync).
//
// `haircut_bps` is a fixed-size `[u16; 32]` indexed by the
// `asset_class` discriminant. 32 slots leaves 3x headroom over today's
// 9 known classes (0..8). New asset classes default to 10_000 bps
// (full haircut, no margin credit) until governance lowers them
// explicitly — fail-closed on new types, mirroring `default_haircut_bps`.

/// Global governance-mutable risk parameter set. One per deployment.
///
/// Phase 4 v1d migration note: `max_staleness_slots` lives at offset 80
/// (re-purposed from the first 8 bytes of the original 32-byte
/// `_reserved` slack). Pre-v1d `RiskParams` PDAs naturally read as
/// `max_staleness_slots == 0`, which the lending program treats as
/// "staleness gate disabled" — backwards-compatible default that an
/// operator can tighten later via `set_max_staleness`. Total size
/// unchanged at 112 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct RiskParams {
    /// Slot at which the params were last mutated (init, any
    /// `set_haircut`, or `set_max_staleness`). Useful for audit +
    /// cache-invalidation.
    pub last_modified_slot: u64,
    /// Collateral haircut in basis points (0–10_000), indexed by
    /// `asset_class` discriminant. Read out-of-range as 10_000.
    pub haircut_bps: [u16; RiskParams::HAIRCUT_TABLE_LEN],
    /// Layout version. Bump on any field-shape migration.
    pub version: u16,
    /// PDA bump.
    pub bump: u8,
    /// Pad to next 8-byte boundary so `max_staleness_slots` is aligned.
    pub _pad: [u8; 5],
    /// Maximum age (in slots) a `PriceFeed.last_updated_slot` may
    /// have before `ssr-lending::open_loan` rejects with
    /// `PRICE_FEED_STALE`. Zero disables the staleness gate — the
    /// pre-v1d default. Set governance-side via `set_max_staleness`.
    pub max_staleness_slots: u64,
    /// Forward-compat slack (shrunk from 32 → 24 bytes when
    /// `max_staleness_slots` was added; total struct size unchanged).
    pub _reserved: [u8; 24],
}

impl RiskParams {
    pub const LEN: usize = core::mem::size_of::<Self>();
    pub const CURRENT_VERSION: u16 = 2;
    /// Width of the `haircut_bps` table. Indexed by `asset_class`
    /// discriminant; entries above the known range stay at the init
    /// default (10_000) until governance updates them.
    pub const HAIRCUT_TABLE_LEN: usize = 32;
    /// Default staleness window for fresh deployments: ~4 min at
    /// Solana mainnet's 0.4-s/slot cadence. Tighten for production,
    /// loosen for low-cadence demo environments.
    pub const DEFAULT_MAX_STALENESS_SLOTS: u64 = 600;

    /// Construct a fresh `RiskParams` seeded from `DEFAULT_HAIRCUTS`
    /// and `DEFAULT_MAX_STALENESS_SLOTS`.
    #[must_use]
    pub fn initial(slot: u64, bump: u8) -> Self {
        Self {
            last_modified_slot: slot,
            haircut_bps: DEFAULT_HAIRCUTS,
            version: Self::CURRENT_VERSION,
            bump,
            _pad: [0; 5],
            max_staleness_slots: Self::DEFAULT_MAX_STALENESS_SLOTS,
            _reserved: [0; 24],
        }
    }

    /// Bounds-checked lookup. Returns 10_000 (full haircut, no credit)
    /// for class IDs beyond the table — same fail-closed shape as
    /// `default_haircut_bps`.
    #[must_use]
    pub fn haircut_for(&self, class: u8) -> u16 {
        let idx = class as usize;
        if idx < Self::HAIRCUT_TABLE_LEN {
            self.haircut_bps[idx]
        } else {
            10_000
        }
    }
}

// ─── PriceFeed (Phase 4 v1d: oracle-priced cross-margin) ────────────────-
//
// One PDA per priced mint at `seeds::PRICE_FEED + mint`, owned by
// `ssr-compliance`. Allocated by super-admin via `register_price_feed`
// (which fixes the mint, its decimals, and the initial price);
// refreshed by the `oracle_operator` role via `update_price` (which
// only touches `price_micro_usd` + `last_updated_slot`). Mint
// decimals are stored here (rather than re-read from the mint
// account at every margin check) so the lending program's
// `enforce_margin` only needs the `PriceFeed` to convert a balance
// into micro-USD — one fewer account per position in an already
// tight account-budget envelope.
//
// `price_micro_usd` interprets as "micro-USD (10⁻⁶ USD) per single
// native unit of the mint". A token with 6 decimals priced at $1.00
// has `price_micro_usd = 1_000_000`. A bond with 4 decimals priced
// at $1.05 has `price_micro_usd = 1_050_000`. The model is
// intentionally unit-of-account-implicit: nothing requires the
// numéraire to be a deployed mint — it's just a shared scale.

/// Per-mint price feed. One PDA per priced mint.
///
/// Phase 4 v1f migration note: `pyth_source` lives at offset 56
/// (re-purposed from the trailing 32-byte `_reserved` slack). Pre-v1f
/// `PriceFeed` PDAs naturally read as `pyth_source == [0; 32]`,
/// which the lending program treats as "not bound to a Pyth
/// account" — manual `update_price` continues to work, and
/// `update_price_from_pyth` rejects with `PRICE_FEED_NOT_PYTH_BOUND`
/// until super-admin sets the source via `bind_price_feed_to_pyth`.
/// Total size unchanged at 88 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct PriceFeed {
    /// Slot at which the feed was last touched (register or any
    /// update). The lending program compares
    /// `clock.slot − last_updated_slot` against
    /// `RiskParams.max_staleness_slots` and rejects stale feeds.
    pub last_updated_slot: u64,
    /// Mint this feed prices. Pinned at allocation; mismatch against
    /// the PDA seed is a `PRICE_FEED_PDA_MISMATCH` rejection.
    pub mint: [u8; 32],
    /// Current price in micro-USD per single native unit of the mint.
    pub price_micro_usd: u64,
    /// Mint decimals captured at registration. The lending program
    /// uses this to convert a `Position::amount_deposited` (native
    /// units) into micro-USD: `balance × price_micro_usd / 10^decimals`.
    pub mint_decimals: u8,
    /// PDA bump.
    pub bump: u8,
    /// Layout version.
    pub version: u16,
    /// Pad to next 8-byte boundary so `pyth_source` is aligned.
    pub _pad: [u8; 4],
    /// Phase 4 v1f: the on-chain Pyth `PriceUpdateV2` account this
    /// feed mirrors, or `[0; 32]` if the feed is fed manually via
    /// `update_price`. `bind_price_feed_to_pyth` (super-admin) sets
    /// it; `update_price_from_pyth` (oracle_operator) requires this
    /// to be non-zero and asserts the passed Pyth account matches
    /// exactly. Trust model: super-admin is responsible for binding
    /// to a real Pyth account at registration time — the program
    /// doesn't validate the source's owner program ID. Manual
    /// `update_price` continues to work regardless of binding (for
    /// fallback when Pyth is down).
    pub pyth_source: [u8; 32],
}

impl PriceFeed {
    pub const LEN: usize = core::mem::size_of::<Self>();
    pub const CURRENT_VERSION: u16 = 2;

    /// Construct a freshly-registered feed. `pyth_source` defaults
    /// to `[0; 32]` (unbound — see field docs).
    #[must_use]
    pub fn initial(
        mint: [u8; 32],
        price_micro_usd: u64,
        mint_decimals: u8,
        slot: u64,
        bump: u8,
    ) -> Self {
        Self {
            last_updated_slot: slot,
            mint,
            price_micro_usd,
            mint_decimals,
            bump,
            version: Self::CURRENT_VERSION,
            _pad: [0; 4],
            pyth_source: [0; 32],
        }
    }

    /// True if `bind_price_feed_to_pyth` has set a non-zero source.
    #[must_use]
    pub fn is_pyth_bound(&self) -> bool {
        self.pyth_source != [0u8; 32]
    }
}

// ─── PythConfig (Phase 4 v1g: oracle owner-validation gate) ─────────────-
//
// Global PDA at `seeds::PYTH_CONFIG` holding the expected Pyth
// Receiver program ID. Allocated once via `initialize_pyth_config`
// (super-admin) and updatable via `set_pyth_program_id`. When
// present, `bind_price_feed_to_pyth` and `update_price_from_pyth`
// verify that the passed Pyth account's owner matches this program
// ID — removing v1f's "trust the super-admin's bind" assumption.
//
// Backwards-compatible default: if the PDA isn't allocated,
// owner-validation is skipped and the program falls back to v1f
// behavior. This lets pre-v1g deployments keep running while
// operators choose when to migrate (init the PDA → all binds and
// updates now validated).

/// Global oracle-source registry (currently just the Pyth Receiver
/// program ID). One PDA per deployment.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct PythConfig {
    /// Slot at which the config was last touched (init or any
    /// `set_pyth_program_id`).
    pub last_modified_slot: u64,
    /// Expected owner program ID for any account bound via
    /// `bind_price_feed_to_pyth`. Per Pyth's deployment docs:
    /// `rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ` on mainnet/devnet
    /// — but the field is configurable so per-deployment overrides
    /// (mocks, alternative Pyth-format providers) work.
    pub pyth_program_id: [u8; 32],
    /// Layout version.
    pub version: u16,
    /// PDA bump.
    pub bump: u8,
    /// Pad to next 8-byte boundary so `_reserved` is aligned.
    pub _pad: [u8; 5],
    /// Forward-compat slack.
    pub _reserved: [u8; 32],
}

impl PythConfig {
    pub const LEN: usize = core::mem::size_of::<Self>();
    pub const CURRENT_VERSION: u16 = 1;

    /// Construct a freshly-initialized config.
    #[must_use]
    pub fn initial(pyth_program_id: [u8; 32], slot: u64, bump: u8) -> Self {
        Self {
            last_modified_slot: slot,
            pyth_program_id,
            version: Self::CURRENT_VERSION,
            bump,
            _pad: [0; 5],
            _reserved: [0; 32],
        }
    }
}

// ─── PDA seed prefixes ───────────────────────────────────────────────────
//
// All SSR programs use these seed prefixes to derive their PDAs. Keeping
// them in the shared types crate prevents the on-chain program and the
// off-chain SDK from drifting on seed strings.

pub mod seeds {
    /// Seed prefix for the global compliance registry PDA. Combined with
    /// nothing else — there is exactly one registry per deployment.
    pub const REGISTRY: &[u8] = b"registry";
    /// Seed prefix for per-participant `AccountRecord` PDAs. Combined
    /// with the participant's 32-byte pubkey.
    pub const ACCOUNT_RECORD: &[u8] = b"account_record";
    /// Seed prefix for the SPL Transfer-Hook Interface
    /// `ExtraAccountMetaList` PDA. Per the SPL spec the PDA seeds are
    /// `[b"extra-account-metas", mint_pubkey]`. Mismatching this string
    /// silently breaks Token-2022 integration (the token program will
    /// not find the meta list), so it is locked to the SPL-required
    /// value and verified at test time.
    pub const EXTRA_META_LIST: &[u8] = b"extra-account-metas";
    /// Seed prefix for per-asset `Vault` PDAs. Combined with the
    /// asset's mint pubkey: `[b"vault", mint]`. One canonical vault
    /// per (deployment, mint).
    pub const VAULT: &[u8] = b"vault";
    /// Seed prefix for per-depositor `Position` PDAs. Combined with
    /// the vault PDA and the depositor pubkey:
    /// `[b"position", vault, depositor]`.
    pub const POSITION: &[u8] = b"position";
    /// Seed prefix for per-trade `Repo` PDAs. Combined with the lock
    /// authority pubkey (typically the `Repo` PDA itself before
    /// derivation, so we use a nonce instead) — see ssr-repo for the
    /// concrete seed schedule:
    /// `[b"repo", borrower, lender, collateral_vault, cash_vault, nonce]`.
    pub const REPO: &[u8] = b"repo";
    /// Seed prefix for per-loan `Loan` PDAs — see ssr-lending for the
    /// concrete seed schedule:
    /// `[b"loan", borrower, lender, collateral_vault, cash_vault, nonce]`.
    pub const LOAN: &[u8] = b"loan";
    /// Seed prefix for the global `RiskParams` PDA (governance-mutable
    /// collateral haircut table). Combined with nothing else — single
    /// global record per deployment, mirror of `REGISTRY`. Owned by
    /// `ssr-compliance`; super-admin gates init + updates.
    pub const RISK_PARAMS: &[u8] = b"risk-params";
    /// Seed prefix for per-borrower `LoanList` PDAs (Phase 4 v1b —
    /// authoritative list of the borrower's open loans, used by
    /// `ssr-lending::open_loan` to enforce margin against the full
    /// liability set rather than what the caller chooses to disclose).
    /// Combined with the borrower's 32-byte pubkey:
    /// `[b"loan-list", borrower]`. Owned by `ssr-lending`.
    pub const LOAN_LIST: &[u8] = b"loan-list";
    /// Seed prefix for per-mint `PriceFeed` PDAs (Phase 4 v1d —
    /// oracle-priced cross-margin). Combined with the mint's 32-byte
    /// pubkey: `[b"price-feed", mint]`. Owned by `ssr-compliance`;
    /// super-admin allocates via `register_price_feed`; oracle_operator
    /// updates via `update_price`. Prices are quoted in
    /// micro-USD per native unit of the mint.
    pub const PRICE_FEED: &[u8] = b"price-feed";
    /// Seed prefix for the global `PythConfig` PDA (Phase 4 v1g —
    /// Pyth account owner-validation gate). Combined with nothing
    /// else — single global record per deployment. Owned by
    /// `ssr-compliance`; super-admin allocates via
    /// `initialize_pyth_config`.
    pub const PYTH_CONFIG: &[u8] = b"pyth-config";
}

// ─── Status transition policy ────────────────────────────────────────────
//
// Lives in the types crate (not the program) so off-chain admin tooling
// can pre-validate proposed transitions before sending the on-chain tx —
// failing fast on the operator's machine rather than burning a CU budget
// and surfacing the error after the fact.

/// True if a transition from `from` to `to` is permitted by policy.
///
/// Policy at Phase 0b:
///   * `BLOCKED` is terminal — no transitions out (compliance permanent).
///   * Self-transitions are rejected (the caller meant something else).
///   * All other transitions between known statuses are permitted.
///   * Unknown discriminants are rejected on both ends.
#[must_use]
pub fn is_valid_status_transition(from: u8, to: u8) -> bool {
    if !compliance_status::is_known(from) || !compliance_status::is_known(to) {
        return false;
    }
    if from == to {
        return false;
    }
    if from == compliance_status::BLOCKED {
        return false;
    }
    true
}

// ─── Vault + Position (Phase 2: collateral vault primitive) ─────────────
//
// `Vault` is the per-asset accounting head — one PDA per
// `(deployment, asset_mint)` holding aggregate balances and admin
// authority. The actual Token-2022 holdings live in the vault PDA's
// canonical ATA (SPL associated-token-account, owner = vault PDA), not
// in the `Vault` account itself.
//
// `Position` is a per-depositor record under a given vault — one PDA
// per `(vault, depositor)` tracking how much that depositor has
// deposited and how much (in the future) is locked by repo / lending /
// margin wrappers. The lock flow is reserved for Phase 3+; for Phase 2
// only `amount_deposited` moves.

/// Aggregate accounting head for a single asset. PDA seeds:
/// `[seeds::VAULT, mint_pubkey]`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Vault {
    /// Slot at which `total_deposited` or `position_count` last changed.
    pub last_modified_slot: u64,
    /// Asset this vault holds. Locked to the mint pubkey baked into the
    /// vault PDA seeds.
    pub mint: [u8; 32],
    /// Admin authority (set at `init_vault` time, typically the
    /// issuer's multisig). For Phase 2 the admin has no operational
    /// privileges over depositor positions — depositors withdraw on
    /// their own signature. Reserved for future migration / pause /
    /// fee-policy hooks.
    pub admin: [u8; 32],
    /// Aggregate of `Position::amount_deposited` across all open
    /// positions. Invariant: equal to the vault's ATA balance modulo
    /// inflight transfers.
    pub total_deposited: u64,
    /// Number of open `Position` PDAs under this vault. Reserved for
    /// future bookkeeping; programs do not currently enforce a max.
    pub position_count: u32,
    /// Layout version. Bump on any field-shape migration.
    pub version: u16,
    /// PDA bump.
    pub bump: u8,
    /// Asset-class discriminant (see `asset_class` module). Phase 4 v0
    /// reads this to apply per-class haircuts during margin computation.
    /// Pre-Phase-4 vaults read as `asset_class::UNKNOWN` (= 0) and
    /// receive zero margin credit until reinitialized — see
    /// `haircut_bps`.
    pub asset_class: u8,
    /// Forward-compat slack.
    pub _reserved: [u8; 16],
}

impl Vault {
    pub const LEN: usize = core::mem::size_of::<Self>();
    pub const CURRENT_VERSION: u16 = 1;

    /// Construct an empty vault. Programs write the resulting bytes
    /// into the freshly-allocated PDA at `init_vault`. Pass
    /// `asset_class::UNKNOWN` if the issuer didn't tag the mint —
    /// the vault still works for deposits / locks, but contributes
    /// zero credit when included in a margin view.
    #[must_use]
    pub fn initial(
        admin: [u8; 32],
        mint: [u8; 32],
        slot: u64,
        bump: u8,
        asset_class: u8,
    ) -> Self {
        Self {
            last_modified_slot: slot,
            mint,
            admin,
            total_deposited: 0,
            position_count: 0,
            version: Self::CURRENT_VERSION,
            bump,
            asset_class,
            _reserved: [0; 16],
        }
    }
}

/// Per-depositor record under a vault. PDA seeds:
/// `[seeds::POSITION, vault_pubkey, depositor_pubkey]`.
///
/// **Lock model**: only one external "locker" can claim against a
/// position at a time (Phase 3 minimum — Phase 4+ may relax to multi-
/// locker). The locker's pubkey is stamped into `lock_authority` when
/// `locked_amount` transitions from zero, and cleared back to all-zero
/// when `locked_amount` returns to zero. Subsequent additive locks by
/// the same authority are permitted; mixing lockers requires the
/// previous one to fully release first.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Position {
    /// Slot at which `amount_deposited` or `locked_amount` last changed.
    pub last_modified_slot: u64,
    /// The vault this position belongs to.
    pub vault: [u8; 32],
    /// The depositor's wallet pubkey.
    pub depositor: [u8; 32],
    /// Tokens this depositor has deposited and not yet withdrawn.
    pub amount_deposited: u64,
    /// Portion of `amount_deposited` reserved by an external program
    /// (repo / lending / margin wrapper). Available to withdraw:
    /// `amount_deposited.checked_sub(locked_amount)`.
    pub locked_amount: u64,
    /// The authority that placed the lock. Either the all-zero pubkey
    /// (no current lock) or the locker's PDA pubkey. Vault
    /// `unlock_position` verifies the calling signer matches this
    /// field, so an attacker cannot release someone else's lock.
    pub lock_authority: [u8; 32],
    /// PDA bump.
    pub bump: u8,
    /// Pad up to the next 8-byte boundary so `_reserved` is aligned.
    pub _pad: [u8; 7],
    /// Forward-compat slack.
    pub _reserved: [u8; 16],
}

impl Position {
    pub const LEN: usize = core::mem::size_of::<Self>();

    /// Construct a zero-balance, unlocked position. Programs write
    /// this when `deposit` idempotently creates the position on first
    /// call.
    #[must_use]
    pub fn empty(vault: [u8; 32], depositor: [u8; 32], slot: u64, bump: u8) -> Self {
        Self {
            last_modified_slot: slot,
            vault,
            depositor,
            amount_deposited: 0,
            locked_amount: 0,
            lock_authority: [0; 32],
            bump,
            _pad: [0; 7],
            _reserved: [0; 16],
        }
    }

    /// Tokens the depositor can withdraw without unlocking anything.
    #[must_use]
    pub fn available(&self) -> u64 {
        self.amount_deposited.saturating_sub(self.locked_amount)
    }

    /// True if `lock_authority` is all-zero — i.e. no external program
    /// currently holds a lock against this position.
    #[must_use]
    pub fn is_unlocked(&self) -> bool {
        self.lock_authority == [0u8; 32]
    }
}

// ─── Repo (Phase 3: time-bound bilateral lock) ──────────────────────────-

/// Repo agreement state. One PDA per open repo.
///
/// At `open_repo`, both parties' positions (collateral on the borrower
/// side, cash on the lender side) are locked against this Repo PDA as
/// the `lock_authority`. The actual movement of cash and the eventual
/// recovery of collateral on default are handled by the repo program's
/// instructions, not by this struct alone.
///
/// Phase 3 minimum implements `open_repo` (both lock) and `close_repo`
/// (both unlock, after borrower has returned the cash); the post-
/// expiry `default_repo` path (lender claims locked collateral
/// directly) is reserved for Phase 3b.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Repo {
    /// Slot at which `status` last changed.
    pub last_modified_slot: u64,
    /// The borrower (their collateral is locked).
    pub borrower: [u8; 32],
    /// The lender (their cash is locked).
    pub lender: [u8; 32],
    /// Vault holding the borrower's collateral.
    pub collateral_vault: [u8; 32],
    /// Vault holding the lender's cash.
    pub cash_vault: [u8; 32],
    /// Collateral tokens locked.
    pub collateral_amount: u64,
    /// Cash tokens locked.
    pub cash_amount: u64,
    /// Slot at which the repo expires. After this slot `close_repo`
    /// rejects and `default_repo` (Phase 3b) becomes available.
    pub expiry_slot: u64,
    /// Disambiguates repos sharing the same `(borrower, lender,
    /// collateral_vault, cash_vault)` tuple.
    pub nonce: u64,
    /// `repo_status::*`.
    pub status: u8,
    /// PDA bump.
    pub bump: u8,
    /// Pad to next 8-byte boundary.
    pub _pad: [u8; 6],
    /// Forward-compat slack.
    pub _reserved: [u8; 16],
}

/// Status discriminants for `Repo::status`.
pub mod repo_status {
    /// Initial state after `open_repo` succeeds.
    pub const OPEN: u8 = 1;
    /// Borrower successfully repaid before expiry; both sides unlocked.
    pub const CLOSED: u8 = 2;
    /// (Phase 3b) Lender claimed locked collateral after expiry.
    pub const DEFAULTED: u8 = 3;

    #[must_use]
    pub const fn is_known(s: u8) -> bool {
        matches!(s, OPEN | CLOSED | DEFAULTED)
    }
}

impl Repo {
    pub const LEN: usize = core::mem::size_of::<Self>();

    /// Construct a freshly-opened repo. The repo program writes this
    /// at `open_repo` after both `lock_position` CPIs succeed.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn opened(
        borrower: [u8; 32],
        lender: [u8; 32],
        collateral_vault: [u8; 32],
        cash_vault: [u8; 32],
        collateral_amount: u64,
        cash_amount: u64,
        expiry_slot: u64,
        nonce: u64,
        slot: u64,
        bump: u8,
    ) -> Self {
        Self {
            last_modified_slot: slot,
            borrower,
            lender,
            collateral_vault,
            cash_vault,
            collateral_amount,
            cash_amount,
            expiry_slot,
            nonce,
            status: repo_status::OPEN,
            bump,
            _pad: [0; 6],
            _reserved: [0; 16],
        }
    }
}

// ─── Loan (Phase 3: collateralized term loan) ───────────────────────────-
//
// `Loan` represents a bilateral collateralized loan with a fixed
// maturity. Phase 3 minimum mirrors repo's encumbrance model: at
// `open_loan` both parties' positions are locked (borrower's
// collateral, lender's principal) against the `Loan` PDA as
// `lock_authority`; cash flows between parties (drawdown at t=0,
// repayment + interest at t=T) are handled off-chain, the same way
// institutional desks treat encumbrance and cash movement as separate
// concerns. `interest_bps_per_year` is recorded for downstream
// processes (interest computation, audit) but is not enforced on-chain
// in the minimum — Phase 3b adds vault-internal cash transfer + forced
// interest computation + liquidation.

/// Bilateral collateralized loan agreement. One PDA per open loan.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Loan {
    /// Slot at which `status` last changed.
    pub last_modified_slot: u64,
    /// The borrower (their collateral is locked).
    pub borrower: [u8; 32],
    /// The lender (their principal is locked).
    pub lender: [u8; 32],
    /// Vault holding the borrower's collateral.
    pub collateral_vault: [u8; 32],
    /// Vault holding the lender's cash principal.
    pub cash_vault: [u8; 32],
    /// Collateral tokens locked.
    pub collateral_amount: u64,
    /// Cash principal tokens locked.
    pub principal_amount: u64,
    /// Slot at which the loan was opened. Used downstream to compute
    /// accrued interest at repay time.
    pub opened_slot: u64,
    /// Slot at which the loan matures. After this slot `repay_loan`
    /// rejects and the (Phase 3b) `liquidate_loan` path becomes
    /// available.
    pub maturity_slot: u64,
    /// Disambiguates loans sharing the same `(borrower, lender,
    /// collateral_vault, cash_vault)` tuple.
    pub nonce: u64,
    /// Simple interest rate in basis points per year, recorded at
    /// open. Not enforced on-chain in Phase 3 minimum — repay simply
    /// unlocks both sides and off-chain settlement applies the
    /// computed `principal_amount * (slots_elapsed / SLOTS_PER_YEAR)
    /// * rate_bps / 10000` cash adjustment.
    pub interest_bps_per_year: u32,
    /// `loan_status::*`.
    pub status: u8,
    /// PDA bump.
    pub bump: u8,
    /// Pad to next 8-byte boundary.
    pub _pad: [u8; 2],
    /// Forward-compat slack.
    pub _reserved: [u8; 16],
}

/// Status discriminants for `Loan::status`.
pub mod loan_status {
    /// Initial state after `open_loan` succeeds.
    pub const OPEN: u8 = 1;
    /// Borrower successfully repaid before maturity; both sides
    /// unlocked.
    pub const REPAID: u8 = 2;
    /// (Phase 3b) Lender claimed locked collateral after maturity due
    /// to borrower default.
    pub const LIQUIDATED: u8 = 3;

    #[must_use]
    pub const fn is_known(s: u8) -> bool {
        matches!(s, OPEN | REPAID | LIQUIDATED)
    }
}

impl Loan {
    pub const LEN: usize = core::mem::size_of::<Self>();

    /// Construct a freshly-opened loan. The lending program writes this
    /// at `open_loan` after both `lock_position` CPIs succeed.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn opened(
        borrower: [u8; 32],
        lender: [u8; 32],
        collateral_vault: [u8; 32],
        cash_vault: [u8; 32],
        collateral_amount: u64,
        principal_amount: u64,
        maturity_slot: u64,
        nonce: u64,
        interest_bps_per_year: u32,
        slot: u64,
        bump: u8,
    ) -> Self {
        Self {
            last_modified_slot: slot,
            borrower,
            lender,
            collateral_vault,
            cash_vault,
            collateral_amount,
            principal_amount,
            opened_slot: slot,
            maturity_slot,
            nonce,
            interest_bps_per_year,
            status: loan_status::OPEN,
            bump,
            _pad: [0; 2],
            _reserved: [0; 16],
        }
    }
}

// ─── LoanList (Phase 4 v1b: per-borrower open-loan index) ───────────────-
//
// `LoanList` exists to defeat one specific adversarial pattern: a
// borrower under-disclosing their existing liabilities when calling
// `open_loan`, in order to fit a new draw inside the margin gate.
// Pass-account margin checks alone can't prevent this — the caller
// chooses what to pass. With `LoanList`, the lending program owns the
// authoritative set of the borrower's open loans, the margin check
// requires the caller to pass exactly those entries, and PDA-shape
// validation catches any substitution.
//
// One PDA per borrower at `[seeds::LOAN_LIST, borrower]`. The lending
// program creates it lazily at the borrower's first `open_loan`,
// appends on open, and removes (swap-with-last + decrement count) on
// `repay_loan` / `liquidate_loan`. Entries are raw 32-byte `Loan`
// PDA pubkeys — denser than re-deriving from `(lender, vaults,
// nonce)` tuples, and the lookup is a flat scan of at most
// `MAX_ENTRIES`.
//
// `MAX_ENTRIES = 16` caps both rent (one 592-byte account per
// borrower) and the on-tx account budget for the margin pre-check
// (each entry adds ~1 Loan PDA pass; combined with positions + the
// new loan's accounts, 16 already pressures Solana's ~64-key limit).
// Institutional flows tend to be a small set of large loans, not a
// long tail of tiny ones, so the cap is not expected to bind.

/// Per-borrower open-loan index. One PDA per borrower.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct LoanList {
    /// Slot at which the list was last mutated (append or remove).
    pub last_modified_slot: u64,
    /// The borrower this list belongs to. Pinned at allocation; mismatch
    /// against the PDA seed is a `LOAN_LIST_PDA_MISMATCH` rejection.
    pub borrower: [u8; 32],
    /// Number of valid entries in `entries[0..count]`. Slots `[count..]`
    /// are zeroed.
    pub count: u8,
    /// PDA bump.
    pub bump: u8,
    /// Layout version.
    pub version: u16,
    /// Pad to next 8-byte boundary so `entries` lands aligned.
    pub _pad: [u8; 4],
    /// Open-loan `Loan` PDA pubkeys. `entries[..count]` are the live
    /// set; `entries[count..]` hold zeros. Order is not stable —
    /// `remove` uses swap-with-last.
    pub entries: [[u8; 32]; LoanList::MAX_ENTRIES],
    /// Forward-compat slack.
    pub _reserved: [u8; 32],
}

impl LoanList {
    pub const LEN: usize = core::mem::size_of::<Self>();
    pub const CURRENT_VERSION: u16 = 1;
    /// Max simultaneous open loans per borrower. See the module note
    /// for the rationale (rent + account-budget pressure under
    /// Solana's ~64-key tx limit).
    pub const MAX_ENTRIES: usize = 16;

    /// Construct an empty list for `borrower`. The lending program
    /// writes this at the borrower's first `open_loan`.
    #[must_use]
    pub fn empty(borrower: [u8; 32], slot: u64, bump: u8) -> Self {
        Self {
            last_modified_slot: slot,
            borrower,
            count: 0,
            bump,
            version: Self::CURRENT_VERSION,
            _pad: [0; 4],
            entries: [[0u8; 32]; Self::MAX_ENTRIES],
            _reserved: [0; 32],
        }
    }

    /// Push a new loan PDA. Returns `false` if the list is already at
    /// `MAX_ENTRIES` (caller should surface a "too many open loans"
    /// error). Does not check duplicates — the caller is expected to
    /// only push freshly-derived `Loan` PDAs, which are unique by
    /// nonce.
    #[must_use]
    pub fn push(&mut self, loan_pda: [u8; 32]) -> bool {
        if (self.count as usize) >= Self::MAX_ENTRIES {
            return false;
        }
        self.entries[self.count as usize] = loan_pda;
        self.count += 1;
        true
    }

    /// Remove the first entry matching `loan_pda` (swap-with-last,
    /// zero the vacated slot). Returns `true` if an entry was removed,
    /// `false` if the loan wasn't in the list.
    #[must_use]
    pub fn remove(&mut self, loan_pda: &[u8; 32]) -> bool {
        for i in 0..(self.count as usize) {
            if &self.entries[i] == loan_pda {
                let last = (self.count as usize) - 1;
                self.entries[i] = self.entries[last];
                self.entries[last] = [0u8; 32];
                self.count -= 1;
                return true;
            }
        }
        false
    }

    /// Iterator over live entries (`entries[..count]`).
    pub fn iter(&self) -> impl Iterator<Item = &[u8; 32]> {
        self.entries[..self.count as usize].iter()
    }
}

// ─── Compliance check API (Phase 0e: registry-only / composition mode) ──
//
// This module lets *other* on-chain programs verify a participant's
// compliance status without CPI-ing into `ssr-compliance`. The pattern:
//
//   1. The caller (e.g. an SSR DvP wrapper, margin engine, repo program)
//      derives the `AccountRecord` PDA from `seeds::ACCOUNT_RECORD ++
//      participant_pubkey`.
//   2. It requests the PDA as a `readonly` account on its instruction.
//   3. It reads the account data and calls `check_record_bytes(&data)`.
//   4. On `Ok(())` the participant is `VERIFIED`; otherwise the variant
//      tells the caller *why* (suspended / blocked / unverified / corrupt
//      record / unknown status discriminant).
//
// The check is intentionally pinocchio-free so it links into both
// Pinocchio and Anchor BPF programs (and into off-chain Rust SDKs)
// without forcing a Solana toolchain choice on downstream code.

/// Classification returned by the compliance check.
///
/// `Ok(())` means the participant is currently `VERIFIED`. Errors are
/// distinct so caller programs can surface specific reason codes in
/// their own error namespaces without having to re-derive them from a
/// single opaque reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckError {
    /// Buffer is shorter than `AccountRecord::LEN` or fails the Pod cast.
    LayoutInvalid,
    /// `status` byte is outside the recognized discriminant range.
    /// Fail closed — do NOT silently treat as "unverified".
    StatusUnknown,
    /// Status is `UNKNOWN` or `PENDING` — onboarding not complete.
    Unverified,
    /// Status is `SUSPENDED` — temporary hold (e.g. sanctions screening).
    Suspended,
    /// Status is `BLOCKED` — permanent (terminal under policy).
    Blocked,
}

impl AccountRecord {
    /// Decide whether this record's participant is currently cleared.
    /// Pure function on the in-memory struct — no I/O.
    pub fn check_transfer_allowed(&self) -> Result<(), CheckError> {
        if !compliance_status::is_known(self.status) {
            return Err(CheckError::StatusUnknown);
        }
        match self.status {
            compliance_status::VERIFIED => Ok(()),
            compliance_status::SUSPENDED => Err(CheckError::Suspended),
            compliance_status::BLOCKED => Err(CheckError::Blocked),
            _ => Err(CheckError::Unverified),
        }
    }
}

/// Borrow an `AccountRecord` out of a raw byte buffer.
///
/// Returns `Err(LayoutInvalid)` if the buffer is shorter than
/// `AccountRecord::LEN`, mis-aligned, or otherwise fails the Pod cast.
/// Excess trailing bytes are ignored so callers can pass account data
/// from accounts allocated larger than `LEN` (forward-compat).
pub fn read_account_record(bytes: &[u8]) -> Result<&AccountRecord, CheckError> {
    if bytes.len() < AccountRecord::LEN {
        return Err(CheckError::LayoutInvalid);
    }
    bytemuck::try_from_bytes(&bytes[..AccountRecord::LEN]).map_err(|_| CheckError::LayoutInvalid)
}

/// Convenience: read the record from a raw buffer and check transfer
/// permission in one call. Equivalent to
/// `read_account_record(bytes)?.check_transfer_allowed()`.
pub fn check_record_bytes(bytes: &[u8]) -> Result<(), CheckError> {
    read_account_record(bytes)?.check_transfer_allowed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_record_size_is_stable() {
        // Lock the layout: changing this number requires an explicit
        // versioned migration, not a silent layout edit.
        assert_eq!(AccountRecord::LEN, 56);
    }

    #[test]
    fn registry_size_is_stable() {
        // Phase 4 v1d re-purposes the trailing 32-byte `_reserved`
        // slack as `oracle_operator`. Total size unchanged at 144.
        assert_eq!(Registry::LEN, 144);
        assert_eq!(core::mem::offset_of!(Registry, oracle_operator), 112);
    }

    #[test]
    fn registry_initial_seeds_all_roles_from_super_admin() {
        let r = Registry::initial([3; 32], 0, 254);
        assert_eq!(r.super_admin, [3; 32]);
        assert_eq!(r.onboard_operator, [3; 32]);
        assert_eq!(r.status_operator, [3; 32]);
        assert_eq!(r.oracle_operator, [3; 32], "oracle role defaults to super_admin");
        assert_eq!(r.operator_for(role::ORACLE), Some(&[3u8; 32]));
        assert_eq!(r.operator_for(role::ONBOARD), Some(&[3u8; 32]));
        assert_eq!(r.operator_for(role::STATUS), Some(&[3u8; 32]));
        assert_eq!(r.operator_for(99), None, "unknown role returns None");
    }

    #[test]
    fn risk_params_size_and_haircut_offset_are_stable() {
        // 8 (slot) + 64 (haircut_bps = 32 * u16) + 2 (version) + 1
        // (bump) + 5 (pad) + 8 (max_staleness_slots) + 24 (reserved)
        // = 112. Size unchanged from v1c — `max_staleness_slots`
        // was carved out of the original 32-byte `_reserved`.
        assert_eq!(RiskParams::LEN, 112);
        assert_eq!(core::mem::offset_of!(RiskParams, haircut_bps), 8);
        assert_eq!(core::mem::offset_of!(RiskParams, max_staleness_slots), 80);
        assert_eq!(RiskParams::HAIRCUT_TABLE_LEN, 32);
    }

    #[test]
    fn risk_params_initial_matches_default_haircuts() {
        let rp = RiskParams::initial(123, 254);
        assert_eq!(rp.haircut_bps, DEFAULT_HAIRCUTS);
        assert_eq!(rp.version, RiskParams::CURRENT_VERSION);
        assert_eq!(rp.bump, 254);
        assert_eq!(rp.last_modified_slot, 123);
        // v1d: max_staleness_slots is seeded to the default at init.
        assert_eq!(
            rp.max_staleness_slots,
            RiskParams::DEFAULT_MAX_STALENESS_SLOTS,
        );
        // Spot-check a few entries against the documented intent so a
        // reorder of asset_class discriminants surfaces here.
        assert_eq!(rp.haircut_for(asset_class::STABLECOIN), 0);
        assert_eq!(rp.haircut_for(asset_class::EQUITY), 3_000);
        assert_eq!(rp.haircut_for(asset_class::UNKNOWN), 10_000);
        // Above the table → full haircut (fail-closed).
        assert_eq!(rp.haircut_for(31), 10_000);
        assert_eq!(rp.haircut_for(255), 10_000);
    }

    #[test]
    fn price_feed_size_and_layout_are_stable() {
        // 8 (slot) + 32 (mint) + 8 (price) + 1 (decimals) + 1 (bump)
        // + 2 (version) + 4 (pad) + 32 (pyth_source) = 88. Total
        // size unchanged from v1d — `pyth_source` re-purposed the
        // original 32-byte `_reserved` slack.
        assert_eq!(PriceFeed::LEN, 88);
        assert_eq!(core::mem::offset_of!(PriceFeed, mint), 8);
        assert_eq!(core::mem::offset_of!(PriceFeed, price_micro_usd), 40);
        assert_eq!(core::mem::offset_of!(PriceFeed, mint_decimals), 48);
        assert_eq!(core::mem::offset_of!(PriceFeed, pyth_source), 56);
    }

    #[test]
    fn price_feed_initial_has_expected_shape() {
        let pf = PriceFeed::initial([7; 32], 1_050_000, 6, 12345, 253);
        assert_eq!(pf.mint, [7; 32]);
        assert_eq!(pf.price_micro_usd, 1_050_000);
        assert_eq!(pf.mint_decimals, 6);
        assert_eq!(pf.last_updated_slot, 12345);
        assert_eq!(pf.bump, 253);
        assert_eq!(pf.version, PriceFeed::CURRENT_VERSION);
        // v1f: fresh feed isn't bound to Pyth until super-admin runs
        // `bind_price_feed_to_pyth`.
        assert_eq!(pf.pyth_source, [0; 32]);
        assert!(!pf.is_pyth_bound());
    }

    #[test]
    fn price_feed_is_pyth_bound_flips_on_nonzero_source() {
        let mut pf = PriceFeed::initial([1; 32], 0, 6, 0, 0);
        assert!(!pf.is_pyth_bound());
        pf.pyth_source[0] = 1;
        assert!(pf.is_pyth_bound());
        pf.pyth_source = [0; 32];
        assert!(!pf.is_pyth_bound());
    }

    #[test]
    fn pyth_config_size_and_layout_are_stable() {
        // 8 (slot) + 32 (program_id) + 2 (version) + 1 (bump)
        // + 5 (pad) + 32 (reserved) = 80.
        assert_eq!(PythConfig::LEN, 80);
        assert_eq!(core::mem::offset_of!(PythConfig, pyth_program_id), 8);
    }

    #[test]
    fn pyth_config_initial_has_expected_shape() {
        let pc = PythConfig::initial([9; 32], 42, 253);
        assert_eq!(pc.pyth_program_id, [9; 32]);
        assert_eq!(pc.last_modified_slot, 42);
        assert_eq!(pc.bump, 253);
        assert_eq!(pc.version, PythConfig::CURRENT_VERSION);
    }

    #[test]
    fn role_oracle_is_known() {
        assert!(role::is_known(role::ONBOARD));
        assert!(role::is_known(role::STATUS));
        assert!(role::is_known(role::ORACLE));
        assert!(!role::is_known(3));
        assert!(!role::is_known(255));
    }

    #[test]
    fn default_haircut_bps_matches_initial_table() {
        // The CLI fallback path must agree with what a freshly-init'd
        // RiskParams PDA would return — otherwise pre- and post-init
        // demos disagree on the same vault's haircut.
        let rp = RiskParams::initial(0, 0);
        for c in 0u8..=255 {
            assert_eq!(default_haircut_bps(c), rp.haircut_for(c));
        }
    }

    #[test]
    fn pending_record_has_expected_shape() {
        let r = AccountRecord::pending([7; 32], jurisdiction::JP, 12345, 255);
        assert_eq!(r.status, compliance_status::PENDING);
        assert_eq!(r.jurisdiction, jurisdiction::JP);
        assert_eq!(r.updated_at_slot, 12345);
        assert_eq!(r.bump, 255);
        assert!(!r.is_verified());
        assert!(!r.is_accredited());
    }

    #[test]
    fn flags_set_and_query() {
        let mut r = AccountRecord::pending([0; 32], jurisdiction::JP, 0, 0);
        r.flags = flags::ACCREDITED | flags::REGULATED_ENTITY;
        assert!(r.is_accredited());
        assert_eq!(r.flags & flags::REGULATED_ENTITY, flags::REGULATED_ENTITY);
    }

    #[test]
    fn compliance_status_is_known_round_trip() {
        for s in [
            compliance_status::UNKNOWN,
            compliance_status::PENDING,
            compliance_status::VERIFIED,
            compliance_status::SUSPENDED,
            compliance_status::BLOCKED,
        ] {
            assert!(compliance_status::is_known(s));
        }
        assert!(!compliance_status::is_known(99));
    }

    #[test]
    fn initial_registry_assigns_super_admin_to_all_roles() {
        let r = Registry::initial([3; 32], 7, 254);
        assert_eq!(r.version, Registry::CURRENT_VERSION);
        assert_eq!(r.super_admin, [3; 32]);
        assert_eq!(r.onboard_operator, [3; 32]);
        assert_eq!(r.status_operator, [3; 32]);
        assert_eq!(r.last_modified_slot, 7);
        assert_eq!(r.bump, 254);
    }

    #[test]
    fn registry_operator_for_dispatches_correctly() {
        let mut r = Registry::initial([1; 32], 0, 0);
        r.onboard_operator = [2; 32];
        r.status_operator = [3; 32];
        assert_eq!(r.operator_for(role::ONBOARD), Some(&[2; 32]));
        assert_eq!(r.operator_for(role::STATUS), Some(&[3; 32]));
        assert_eq!(r.operator_for(99), None);
    }

    #[test]
    fn status_transitions_blocked_is_terminal() {
        assert!(!is_valid_status_transition(
            compliance_status::BLOCKED,
            compliance_status::VERIFIED
        ));
        assert!(!is_valid_status_transition(
            compliance_status::BLOCKED,
            compliance_status::PENDING
        ));
    }

    #[test]
    fn status_transitions_self_is_rejected() {
        for s in [
            compliance_status::PENDING,
            compliance_status::VERIFIED,
            compliance_status::SUSPENDED,
        ] {
            assert!(!is_valid_status_transition(s, s));
        }
    }

    #[test]
    fn status_transitions_unknown_endpoints_rejected() {
        assert!(!is_valid_status_transition(99, compliance_status::VERIFIED));
        assert!(!is_valid_status_transition(compliance_status::VERIFIED, 99));
    }

    #[test]
    fn status_transitions_permitted_paths() {
        // Onboarding flow: PENDING → VERIFIED
        assert!(is_valid_status_transition(
            compliance_status::PENDING,
            compliance_status::VERIFIED
        ));
        // Temporary suspension and unsuspension
        assert!(is_valid_status_transition(
            compliance_status::VERIFIED,
            compliance_status::SUSPENDED
        ));
        assert!(is_valid_status_transition(
            compliance_status::SUSPENDED,
            compliance_status::VERIFIED
        ));
        // Escalation to permanent block
        assert!(is_valid_status_transition(
            compliance_status::SUSPENDED,
            compliance_status::BLOCKED
        ));
        assert!(is_valid_status_transition(
            compliance_status::VERIFIED,
            compliance_status::BLOCKED
        ));
    }

    // ── Compliance check API (registry-only / composition mode) ──

    use bytemuck::bytes_of;

    fn rec(status: u8) -> AccountRecord {
        let mut r = AccountRecord::pending([9; 32], jurisdiction::JP, 5, 0);
        r.status = status;
        r
    }

    #[test]
    fn check_transfer_allowed_verified() {
        assert!(rec(compliance_status::VERIFIED).check_transfer_allowed().is_ok());
    }

    #[test]
    fn check_transfer_allowed_status_classes() {
        assert_eq!(
            rec(compliance_status::SUSPENDED).check_transfer_allowed(),
            Err(CheckError::Suspended)
        );
        assert_eq!(
            rec(compliance_status::BLOCKED).check_transfer_allowed(),
            Err(CheckError::Blocked)
        );
        assert_eq!(
            rec(compliance_status::PENDING).check_transfer_allowed(),
            Err(CheckError::Unverified)
        );
        assert_eq!(
            rec(compliance_status::UNKNOWN).check_transfer_allowed(),
            Err(CheckError::Unverified)
        );
    }

    #[test]
    fn check_transfer_allowed_out_of_range_fails_closed() {
        // Unknown discriminants must surface as `StatusUnknown`, never
        // be silently mapped to `Unverified`.
        let mut r = rec(compliance_status::VERIFIED);
        r.status = 99;
        assert_eq!(r.check_transfer_allowed(), Err(CheckError::StatusUnknown));
    }

    #[test]
    fn read_account_record_layout_validation() {
        // Too short → LayoutInvalid
        assert_eq!(read_account_record(&[]).err(), Some(CheckError::LayoutInvalid));
        assert_eq!(
            read_account_record(&[0u8; AccountRecord::LEN - 1]).err(),
            Some(CheckError::LayoutInvalid)
        );
        // Exact length parses cleanly.
        let r = rec(compliance_status::VERIFIED);
        let buf = bytes_of(&r);
        let parsed = read_account_record(buf).unwrap();
        assert_eq!(parsed.status, compliance_status::VERIFIED);
    }

    #[test]
    fn read_account_record_ignores_trailing_bytes() {
        let r = rec(compliance_status::VERIFIED);
        let mut buf = [0u8; AccountRecord::LEN + 64];
        buf[..AccountRecord::LEN].copy_from_slice(bytes_of(&r));
        let parsed = read_account_record(&buf).unwrap();
        assert_eq!(parsed.status, compliance_status::VERIFIED);
    }

    #[test]
    fn check_record_bytes_round_trip() {
        let r = rec(compliance_status::VERIFIED);
        assert!(check_record_bytes(bytes_of(&r)).is_ok());

        let r = rec(compliance_status::BLOCKED);
        assert_eq!(check_record_bytes(bytes_of(&r)), Err(CheckError::Blocked));
    }

    // ── Vault + Position (Phase 2) ──

    #[test]
    fn vault_size_is_stable() {
        assert_eq!(Vault::LEN, 104);
    }

    #[test]
    fn position_size_is_stable() {
        // Phase 3 bumped this from 104 → 144 by adding `lock_authority`.
        // Any further changes require a versioned migration; do not
        // silently edit this number.
        assert_eq!(Position::LEN, 144);
    }

    #[test]
    fn repo_size_is_stable() {
        // 8 (slot) + 32*4 (parties + vaults) + 8*4 (amounts + expiry + nonce)
        // + 1 (status) + 1 (bump) + 6 (pad) + 16 (reserved) = 192.
        assert_eq!(Repo::LEN, 192);
    }

    // ssr-cli's `margin show` netting uses these offsets in
    // getProgramAccounts memcmp filters. Any reorder that shifts them
    // silently breaks the off-chain margin view; pin them here.
    #[test]
    fn repo_borrower_lender_offsets_are_stable() {
        assert_eq!(core::mem::offset_of!(Repo, borrower), 8);
        assert_eq!(core::mem::offset_of!(Repo, lender), 40);
    }

    #[test]
    fn position_empty_is_unlocked() {
        let p = Position::empty([1; 32], [2; 32], 0, 0);
        assert!(p.is_unlocked());
        assert_eq!(p.lock_authority, [0; 32]);
    }

    #[test]
    fn position_is_unlocked_only_when_authority_zero() {
        let mut p = Position::empty([0; 32], [0; 32], 0, 0);
        assert!(p.is_unlocked());
        p.lock_authority[0] = 1;
        assert!(!p.is_unlocked());
        p.lock_authority = [0; 32];
        assert!(p.is_unlocked());
    }

    #[test]
    fn repo_opened_status_open() {
        let r = Repo::opened([1; 32], [2; 32], [3; 32], [4; 32], 1000, 999, 5_000_000, 7, 100, 254);
        assert_eq!(r.status, repo_status::OPEN);
        assert_eq!(r.borrower, [1; 32]);
        assert_eq!(r.lender, [2; 32]);
        assert_eq!(r.collateral_vault, [3; 32]);
        assert_eq!(r.cash_vault, [4; 32]);
        assert_eq!(r.collateral_amount, 1000);
        assert_eq!(r.cash_amount, 999);
        assert_eq!(r.expiry_slot, 5_000_000);
        assert_eq!(r.nonce, 7);
        assert_eq!(r.last_modified_slot, 100);
        assert_eq!(r.bump, 254);
    }

    #[test]
    fn repo_status_is_known() {
        assert!(repo_status::is_known(repo_status::OPEN));
        assert!(repo_status::is_known(repo_status::CLOSED));
        assert!(repo_status::is_known(repo_status::DEFAULTED));
        assert!(!repo_status::is_known(0));
        assert!(!repo_status::is_known(99));
    }

    #[test]
    fn loan_size_is_stable() {
        // 8 (slot) + 32*4 (parties + vaults) + 8 (collateral) + 8 (principal)
        // + 8 (opened) + 8 (maturity) + 8 (nonce) + 4 (interest_bps) + 1 (status)
        // + 1 (bump) + 2 (pad) + 16 (reserved) = 200.
        assert_eq!(Loan::LEN, 200);
    }

    #[test]
    fn loan_borrower_lender_offsets_are_stable() {
        assert_eq!(core::mem::offset_of!(Loan, borrower), 8);
        assert_eq!(core::mem::offset_of!(Loan, lender), 40);
    }

    #[test]
    fn loan_list_size_and_entries_offset_are_stable() {
        // 8 (slot) + 32 (borrower) + 1 (count) + 1 (bump) + 2 (version)
        // + 4 (pad) + 16*32 (entries) + 32 (reserved) = 592.
        assert_eq!(LoanList::LEN, 592);
        // ssr-lending iterates `entries[..count]` directly and the
        // margin pre-check verifies each passed Loan PDA against an
        // entry slot at this offset. Drift here silently mismatches.
        assert_eq!(core::mem::offset_of!(LoanList, entries), 48);
        assert_eq!(LoanList::MAX_ENTRIES, 16);
    }

    #[test]
    fn loan_list_push_and_remove_swap_with_last() {
        let mut ll = LoanList::empty([7; 32], 100, 254);
        assert_eq!(ll.count, 0);
        assert!(ll.push([1; 32]));
        assert!(ll.push([2; 32]));
        assert!(ll.push([3; 32]));
        assert_eq!(ll.count, 3);
        assert_eq!(ll.entries[0], [1u8; 32]);
        assert_eq!(ll.entries[1], [2u8; 32]);
        assert_eq!(ll.entries[2], [3u8; 32]);

        // Remove from the middle → swap-with-last: [1, 3, _, ...].
        assert!(ll.remove(&[2u8; 32]));
        assert_eq!(ll.count, 2);
        assert_eq!(ll.entries[0], [1u8; 32]);
        assert_eq!(ll.entries[1], [3u8; 32]);
        assert_eq!(ll.entries[2], [0u8; 32], "vacated slot zeroed");

        // Remove non-existent.
        assert!(!ll.remove(&[42u8; 32]));
        assert_eq!(ll.count, 2);

        // Remove last entry.
        assert!(ll.remove(&[3u8; 32]));
        assert_eq!(ll.count, 1);
        assert_eq!(ll.entries[0], [1u8; 32]);
        assert_eq!(ll.entries[1], [0u8; 32]);

        // `iter()` should now yield exactly one live entry.
        let mut seen = 0usize;
        for e in ll.iter() {
            assert_eq!(e, &[1u8; 32]);
            seen += 1;
        }
        assert_eq!(seen, 1);
    }

    #[test]
    fn loan_list_push_rejects_beyond_max() {
        let mut ll = LoanList::empty([7; 32], 0, 0);
        for i in 0..LoanList::MAX_ENTRIES {
            assert!(ll.push([i as u8; 32]));
        }
        assert_eq!(ll.count as usize, LoanList::MAX_ENTRIES);
        // The 17th push must fail without mutating count or entries.
        assert!(!ll.push([99u8; 32]));
        assert_eq!(ll.count as usize, LoanList::MAX_ENTRIES);
        assert_eq!(ll.entries[0], [0u8; 32]);
    }

    #[test]
    fn loan_opened_status_open() {
        let l = Loan::opened(
            [1; 32], [2; 32], [3; 32], [4; 32],
            1_000_000, 800_000, 6_000_000, 11, 500, 200, 254,
        );
        assert_eq!(l.status, loan_status::OPEN);
        assert_eq!(l.borrower, [1; 32]);
        assert_eq!(l.lender, [2; 32]);
        assert_eq!(l.collateral_vault, [3; 32]);
        assert_eq!(l.cash_vault, [4; 32]);
        assert_eq!(l.collateral_amount, 1_000_000);
        assert_eq!(l.principal_amount, 800_000);
        assert_eq!(l.maturity_slot, 6_000_000);
        assert_eq!(l.nonce, 11);
        assert_eq!(l.interest_bps_per_year, 500);
        assert_eq!(l.opened_slot, 200);
        assert_eq!(l.last_modified_slot, 200);
        assert_eq!(l.bump, 254);
    }

    #[test]
    fn loan_status_is_known() {
        assert!(loan_status::is_known(loan_status::OPEN));
        assert!(loan_status::is_known(loan_status::REPAID));
        assert!(loan_status::is_known(loan_status::LIQUIDATED));
        assert!(!loan_status::is_known(0));
        assert!(!loan_status::is_known(99));
    }

    #[test]
    fn vault_initial_has_expected_shape() {
        let v = Vault::initial([1; 32], [2; 32], 100, 254, asset_class::EQUITY);
        assert_eq!(v.admin, [1; 32]);
        assert_eq!(v.mint, [2; 32]);
        assert_eq!(v.last_modified_slot, 100);
        assert_eq!(v.bump, 254);
        assert_eq!(v.total_deposited, 0);
        assert_eq!(v.position_count, 0);
        assert_eq!(v.version, Vault::CURRENT_VERSION);
        assert_eq!(v.asset_class, asset_class::EQUITY);
    }

    #[test]
    fn haircut_known_classes_are_below_full_haircut() {
        // Spot-check that recognized classes give *some* credit and
        // UNKNOWN gets none. The exact figures will move; the
        // invariant we pin is the ordering: cash-equivalent < bond <
        // equity < real_asset, and UNKNOWN is the worst case.
        assert_eq!(haircut_bps(asset_class::STABLECOIN), 0);
        assert!(haircut_bps(asset_class::SOVEREIGN_BOND) < haircut_bps(asset_class::EQUITY));
        assert!(haircut_bps(asset_class::EQUITY) < haircut_bps(asset_class::REAL_ASSET));
        assert_eq!(haircut_bps(asset_class::UNKNOWN), 10_000);
        assert_eq!(haircut_bps(99), 10_000); // out-of-range
    }

    #[test]
    fn position_empty_has_expected_shape() {
        let p = Position::empty([3; 32], [4; 32], 200, 253);
        assert_eq!(p.vault, [3; 32]);
        assert_eq!(p.depositor, [4; 32]);
        assert_eq!(p.last_modified_slot, 200);
        assert_eq!(p.bump, 253);
        assert_eq!(p.amount_deposited, 0);
        assert_eq!(p.locked_amount, 0);
        assert_eq!(p.lock_authority, [0; 32]);
        assert_eq!(p.available(), 0);
    }

    #[test]
    fn position_available_subtracts_locked() {
        let mut p = Position::empty([0; 32], [0; 32], 0, 0);
        p.amount_deposited = 1_000;
        p.locked_amount = 300;
        assert_eq!(p.available(), 700);
    }

    #[test]
    fn position_available_saturates_on_overlock() {
        // Defensive: if a future wrapper accidentally locks more than
        // deposited (which is a bug), `available` must saturate at 0
        // rather than underflow. The check_locked_invariant test below
        // covers the in-program invariant.
        let mut p = Position::empty([0; 32], [0; 32], 0, 0);
        p.amount_deposited = 100;
        p.locked_amount = 250;
        assert_eq!(p.available(), 0);
    }
}
