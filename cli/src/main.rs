//! `ssr-cli` — admin and demo surface for SSR compliance and DvP wrapper.
//!
//! Wraps the on-chain instructions that `ssr-compliance` and
//! `ssr-dvp-wrapper` expose so an operator (or a demo running against a
//! local validator) can:
//!
//!   * Bootstrap a compliance registry, register and verify participants,
//!     suspend / block, rotate operational roles.
//!   * Read participant `AccountRecord`s and the global `Registry`
//!     and pretty-print them.
//!   * Derive the wrapper's settlement-authority PDA so external
//!     parties can name it as `settlement_authority` when creating an
//!     SPC DvP.
//!   * Settle a fully-funded SPC SwapDvp through the compliance
//!     wrapper.
//!
//! Configuration loading mirrors the standard Solana CLI:
//!   `--rpc-url <url>` or `SSR_RPC_URL` (env), defaults to
//!   `~/.config/solana/cli/config.yml`'s `json_rpc_url`, finally falls
//!   back to `http://127.0.0.1:8899`.
//!   `--keypair <path>` or `SSR_KEYPAIR`, defaults to that config's
//!   `keypair_path`.

mod compliance_demo;
mod scenario;

use {
    anyhow::{Context, Result, anyhow, bail},
    clap::{Parser, Subcommand},
    serde::Deserialize,
    solana_address::Address,
    solana_commitment_config::CommitmentConfig,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::{Keypair, read_keypair_file},
    solana_message::Message,
    solana_rpc_client::{
        api::{
            config::{RpcAccountInfoConfig, RpcProgramAccountsConfig, UiAccountEncoding},
            filter::{Memcmp, RpcFilterType},
        },
        rpc_client::RpcClient,
    },
    solana_signer::Signer,
    solana_transaction::Transaction,
    ssr_dvp_wrapper::{AUTHORITY_SEED, SPC_DVP_PROGRAM_ID},
    ssr_lending::{BPS_DENOMINATOR, SLOTS_PER_YEAR},
    ssr_types::{
        AccountRecord, CheckError, Loan, LoanList, Position, PriceFeed, PythConfig, Registry,
        Repo, RiskParams, Vault, asset_class, asset_class_label, compliance_status,
        default_haircut_bps, loan_status, repo_status, role, seeds,
    },
    std::{path::PathBuf, str::FromStr},
};

// ─── Top-level CLI ──────────────────────────────────────────────────────-

#[derive(Parser)]
#[command(name = "ssr-cli", about = "Admin + demo surface for SSR")]
struct Cli {
    /// JSON-RPC URL of the cluster to talk to.
    #[arg(
        long,
        env = "SSR_RPC_URL",
        global = true,
        help = "JSON-RPC URL; falls back to ~/.config/solana/cli/config.yml"
    )]
    rpc_url: Option<String>,

    /// Keypair file path (used to sign transactions). Optional for
    /// read-only commands (`*-cli ... state`, `... status`, `derive
    /// ...`, `dvp authority-address`) which only need RPC reads.
    #[arg(
        long,
        env = "SSR_KEYPAIR",
        global = true,
        help = "Keypair file; falls back to ~/.config/solana/cli/config.yml. \
                Optional for read-only commands."
    )]
    keypair: Option<PathBuf>,

    /// Compliance program ID (the deployed `ssr-compliance` program).
    #[arg(
        long,
        env = "SSR_COMPLIANCE_PROGRAM",
        global = true,
        help = "ssr-compliance program ID (base58)"
    )]
    compliance_program: Option<String>,

    /// Wrapper program ID (the deployed `ssr-dvp-wrapper` program).
    #[arg(
        long,
        env = "SSR_WRAPPER_PROGRAM",
        global = true,
        help = "ssr-dvp-wrapper program ID (base58)"
    )]
    wrapper_program: Option<String>,

    /// Vault program ID (the deployed `ssr-vault` program).
    #[arg(
        long,
        env = "SSR_VAULT_PROGRAM",
        global = true,
        help = "ssr-vault program ID (base58)"
    )]
    vault_program: Option<String>,

    /// Repo program ID (the deployed `ssr-repo` program).
    #[arg(
        long,
        env = "SSR_REPO_PROGRAM",
        global = true,
        help = "ssr-repo program ID (base58)"
    )]
    repo_program: Option<String>,

    /// Lending program ID (the deployed `ssr-lending` program).
    #[arg(
        long,
        env = "SSR_LENDING_PROGRAM",
        global = true,
        help = "ssr-lending program ID (base58)"
    )]
    lending_program: Option<String>,

    #[command(subcommand)]
    command: TopCommand,
}

#[derive(Subcommand)]
enum TopCommand {
    /// Compliance registry + per-participant operations.
    #[command(subcommand)]
    Compliance(ComplianceCmd),
    /// DvP wrapper operations.
    #[command(subcommand)]
    Dvp(DvpCmd),
    /// Collateral vault operations.
    #[command(subcommand)]
    Vault(VaultCmd),
    /// Repo (compliance-gated bilateral lock) operations.
    #[command(subcommand)]
    Repo(RepoCmd),
    /// Lending (compliance-gated collateralized term loan) operations.
    #[command(subcommand)]
    Lending(LendingCmd),
    /// Phase 4 cross-margin views (read-only through v1a).
    #[command(subcommand)]
    Margin(MarginCmd),
    /// Pure derivations — no RPC.
    #[command(subcommand)]
    Derive(DeriveCmd),
    /// Sandbox scenario surface — discover, inspect, and walk through
    /// pre-baked institutional flows. See `scenarios/` and
    /// `fabrknt/website/SANDBOX-PATTERN.md`.
    #[command(subcommand)]
    Scenario(ScenarioCmd),
    /// Standalone compliance-gate behaviour demo. Boots a synthetic
    /// 4-participant population (one per status) and prints the
    /// transfer-allowed matrix. Pure Rust against `ssr-types`
    /// primitives — no validator, no LiteSVM, no deployed program.
    /// Same flow the scenario runner v2 path invokes for the
    /// `compliance-gate-demo` scenario.
    ComplianceGateDemo,
}

#[derive(Subcommand)]
enum ScenarioCmd {
    /// List all available scenarios with their headlines.
    List {
        /// Directory containing scenario JSON files. Default: `scenarios/`
        /// relative to the current working directory.
        #[arg(long, default_value = "scenarios")]
        dir: std::path::PathBuf,
    },
    /// Print one scenario's metadata and step summary.
    Show {
        /// Scenario name (file stem without `.json`).
        name: String,
        #[arg(long, default_value = "scenarios")]
        dir: std::path::PathBuf,
    },
    /// Run the scenario: spawn each step as a sub-process (ssr-cli
    /// prefixes auto-route to current_exe; other CLIs spawned by
    /// name). Stdio inherited so step output streams live. Pass
    /// `--dry-run` to print only the step list without executing.
    Run {
        /// Scenario name (file stem without `.json`).
        name: String,
        #[arg(long, default_value = "scenarios")]
        dir: std::path::PathBuf,
        /// Skip embedded execution; print only the step list.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ComplianceCmd {
    /// Create the global registry PDA. Signer becomes the initial super-admin.
    InitRegistry,
    /// Onboard a participant in PENDING status.
    Register {
        #[arg(long)]
        participant: String,
        /// 2-letter ISO 3166-1 alpha-2 code (e.g. JP, US, KY).
        #[arg(long, default_value = "JP")]
        jurisdiction: String,
    },
    /// Promote PENDING/SUSPENDED → VERIFIED.
    Verify {
        #[arg(long)]
        participant: String,
    },
    /// VERIFIED → SUSPENDED (temporary hold).
    Suspend {
        #[arg(long)]
        participant: String,
    },
    /// → BLOCKED. Terminal — policy rejects any future transition out.
    Block {
        #[arg(long)]
        participant: String,
    },
    /// Read a participant's AccountRecord and pretty-print it.
    Status {
        #[arg(long)]
        participant: String,
    },
    /// Read the global registry state.
    ShowRegistry,
    /// Super-admin rotates an operational role to a new pubkey.
    RotateOperator {
        /// Role to rotate: `onboard` or `status`.
        #[arg(long)]
        role: String,
        /// New pubkey for the role.
        #[arg(long)]
        new_pubkey: String,
    },
    /// Allocate the global RiskParams PDA, seeded with the default
    /// haircut table from `ssr_types::DEFAULT_HAIRCUTS`. Signer must
    /// be the registry's super-admin. Idempotent at the protocol
    /// level — re-running rejects with `AccountAlreadyInitialized`.
    InitRiskParams,
    /// Super-admin updates one asset class's haircut. The signer
    /// (global `--keypair`) must match `Registry::super_admin`.
    SetHaircut {
        /// Asset class name (same vocabulary as `vault init
        /// --asset-class`): stablecoin / tokenized_deposit /
        /// sovereign_bond / corporate_bond / equity / fund_unit /
        /// real_asset / commodity / unknown.
        #[arg(long)]
        class: String,
        /// New haircut in basis points (0–10_000). 0 = full credit,
        /// 10_000 = zero credit.
        #[arg(long)]
        bps: u16,
    },
    /// Read the global RiskParams state and pretty-print the haircut
    /// table. Read-only.
    ShowRiskParams,
    /// Super-admin allocates a `PriceFeed` PDA for `mint`, seeded
    /// with an initial price + the mint's decimals. Phase 4 v1d.
    RegisterPriceFeed {
        #[arg(long)]
        mint: String,
        /// Initial price in micro-USD per single native unit of the
        /// mint. E.g., `1_000_000` = $1.00 for any decimals.
        #[arg(long)]
        price_micro_usd: u64,
        /// The mint's `decimals` field. Captured once at registration
        /// so the lending program doesn't have to re-read the mint
        /// account on every margin check.
        #[arg(long)]
        mint_decimals: u8,
    },
    /// Oracle operator writes a fresh price to an existing
    /// `PriceFeed` PDA. The signer (global `--keypair`) must match
    /// `Registry::oracle_operator`.
    UpdatePrice {
        #[arg(long)]
        mint: String,
        #[arg(long)]
        price_micro_usd: u64,
    },
    /// Read a `PriceFeed` and pretty-print it. Read-only.
    ShowPriceFeed {
        #[arg(long)]
        mint: String,
    },
    /// Super-admin updates `RiskParams.max_staleness_slots`. Zero
    /// disables the staleness gate.
    SetMaxStaleness {
        #[arg(long)]
        slots: u64,
    },
    /// Super-admin binds a `PriceFeed` to a Pyth `PriceUpdateV2`
    /// account (Phase 4 v1f — oracle adapter). Pass the all-zero
    /// pubkey to unbind. Once bound, `update-price-from-pyth`
    /// becomes available for this mint.
    BindPriceFeedToPyth {
        #[arg(long)]
        mint: String,
        /// Pyth `PriceUpdateV2` account pubkey. Pass
        /// `11111111111111111111111111111111` to unbind.
        #[arg(long)]
        pyth_source: String,
    },
    /// Oracle operator refreshes a Pyth-bound `PriceFeed` from the
    /// live Pyth account. The CLI doesn't validate the Pyth source
    /// — trust is in the operator-set binding done via
    /// `bind-price-feed-to-pyth`. When the deployment has run
    /// `init-pyth-config`, the CLI auto-attaches that PDA so the
    /// on-chain handler also validates the Pyth account's owner.
    UpdatePriceFromPyth {
        #[arg(long)]
        mint: String,
    },
    /// Super-admin allocates the global `PythConfig` PDA, enabling
    /// owner-validation on every Pyth bind / update (Phase 4 v1g).
    /// Pre-v1g deployments work without this; running it once flips
    /// the deployment into the owner-validated mode.
    InitPythConfig {
        /// Expected owner program ID for Pyth-bound accounts. For
        /// real Pyth deployments, this is the Pyth Receiver
        /// program ID (`rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ`
        /// on Solana mainnet/devnet).
        #[arg(long)]
        pyth_program_id: String,
    },
    /// Super-admin updates `PythConfig.pyth_program_id`.
    SetPythProgramId {
        #[arg(long)]
        pyth_program_id: String,
    },
    /// Read the global `PythConfig` state. Read-only.
    ShowPythConfig,
}

#[derive(Subcommand)]
enum DvpCmd {
    /// Print the wrapper's settlement-authority PDA. External callers
    /// pass this as `settlement_authority` when issuing SPC CreateDvp.
    AuthorityAddress,
    /// Compliance-check both parties and settle a funded SwapDvp.
    Settle {
        /// SPC SwapDvp PDA.
        #[arg(long)]
        swap_dvp: String,
        /// First `leg_a_extras_count` trailing accounts go to leg A's
        /// TransferChecked CPI; the rest go to leg B's. Mirrors SPC's
        /// `leg_a_extras_count: u8` data byte.
        #[arg(long, default_value_t = 0)]
        leg_a_extras_count: u8,
    },
}

#[derive(Subcommand)]
enum VaultCmd {
    /// Allocate the `Vault` PDA for a given Token-2022 mint. The
    /// global `--keypair` is the admin and also pays rent.
    Init {
        #[arg(long)]
        mint: String,
        /// Asset class tag baked into the vault. Determines the
        /// haircut applied when the vault contributes to a cross-margin
        /// view (`ssr-cli margin show`). Names map to discriminants in
        /// `ssr_types::asset_class`. Accepts: `stablecoin`,
        /// `tokenized_deposit`, `sovereign_bond`, `corporate_bond`,
        /// `equity`, `fund_unit`, `real_asset`, `commodity`,
        /// `unknown`. Default `unknown` → zero margin credit.
        #[arg(long, default_value = "unknown")]
        asset_class: String,
    },
    /// Idempotently allocate a zero-balance `Position` PDA so it can
    /// be the destination of a subsequent `lending open` (borrower's
    /// cash side) or `lending liquidate` (lender's collateral side).
    /// The global `--keypair` signs as the position's depositor and
    /// also pays rent. No token movement.
    InitPosition {
        #[arg(long)]
        mint: String,
    },
    /// Deposit into the vault. The global `--keypair` is the
    /// depositor and signs the underlying Token-2022 transfer.
    Deposit {
        #[arg(long)]
        mint: String,
        #[arg(long)]
        amount: u64,
    },
    /// Withdraw from the vault. The global `--keypair` is the
    /// depositor; the vault PDA signs the underlying Token-2022
    /// transfer.
    Withdraw {
        #[arg(long)]
        mint: String,
        #[arg(long)]
        amount: u64,
    },
    /// Read and pretty-print the `Vault` state.
    State {
        #[arg(long)]
        mint: String,
    },
    /// Read and pretty-print a depositor's `Position` state.
    Position {
        #[arg(long)]
        mint: String,
        /// Defaults to the global `--keypair`'s pubkey.
        #[arg(long)]
        depositor: Option<String>,
    },
}

#[derive(Subcommand)]
enum RepoCmd {
    /// Open a bilateral lock. Requires three signers: payer (global
    /// `--keypair`), borrower, and lender. The latter two are loaded
    /// from their own keypair files.
    Open {
        #[arg(long)]
        borrower_keypair: PathBuf,
        #[arg(long)]
        lender_keypair: PathBuf,
        #[arg(long)]
        collateral_mint: String,
        #[arg(long)]
        cash_mint: String,
        #[arg(long)]
        collateral_amount: u64,
        #[arg(long)]
        cash_amount: u64,
        #[arg(long)]
        expiry_slot: u64,
        #[arg(long)]
        nonce: u64,
    },
    /// Close an open repo. Requires the borrower (global `--keypair`)
    /// to sign; lender's pubkey is read from the on-chain state.
    Close {
        #[arg(long)]
        lender: String,
        #[arg(long)]
        collateral_mint: String,
        #[arg(long)]
        cash_mint: String,
        #[arg(long)]
        nonce: u64,
    },
    /// Read and pretty-print the `Repo` state.
    State {
        #[arg(long)]
        borrower: String,
        #[arg(long)]
        lender: String,
        #[arg(long)]
        collateral_mint: String,
        #[arg(long)]
        cash_mint: String,
        #[arg(long)]
        nonce: u64,
    },
}

#[derive(Subcommand)]
enum LendingCmd {
    /// Open a collateralized term loan. Requires three signers: payer
    /// (global `--keypair`), borrower, and lender. The latter two are
    /// loaded from their own keypair files.
    Open {
        #[arg(long)]
        borrower_keypair: PathBuf,
        #[arg(long)]
        lender_keypair: PathBuf,
        #[arg(long)]
        collateral_mint: String,
        #[arg(long)]
        cash_mint: String,
        #[arg(long)]
        collateral_amount: u64,
        #[arg(long)]
        principal_amount: u64,
        #[arg(long)]
        maturity_slot: u64,
        #[arg(long)]
        nonce: u64,
        /// Simple interest rate, basis points per year. Recorded only in
        /// Phase 3 minimum — Phase 3b will enforce `principal_amount *
        /// (slots_elapsed / SLOTS_PER_YEAR) * rate_bps / 10000` at repay.
        #[arg(long, default_value_t = 0)]
        interest_bps_per_year: u32,
    },
    /// Repay an open loan. Requires the borrower (global `--keypair`)
    /// to sign; lender's pubkey is read from the on-chain state.
    Repay {
        #[arg(long)]
        lender: String,
        #[arg(long)]
        collateral_mint: String,
        #[arg(long)]
        cash_mint: String,
        #[arg(long)]
        nonce: u64,
    },
    /// Post-maturity liquidation: lender seizes the borrower's locked
    /// collateral. Requires the lender (global `--keypair`) to sign.
    Liquidate {
        #[arg(long)]
        borrower: String,
        #[arg(long)]
        collateral_mint: String,
        #[arg(long)]
        cash_mint: String,
        #[arg(long)]
        nonce: u64,
    },
    /// Read and pretty-print the `Loan` state.
    State {
        #[arg(long)]
        borrower: String,
        #[arg(long)]
        lender: String,
        #[arg(long)]
        collateral_mint: String,
        #[arg(long)]
        cash_mint: String,
        #[arg(long)]
        nonce: u64,
    },
}

#[derive(Subcommand)]
enum MarginCmd {
    /// Read-only cross-margin view for a single user.
    ///
    /// Two passes:
    ///   1. **Collateral.** For each `--mint` we read the user's
    ///      `Position` and apply the per-vault `asset_class` haircut
    ///      from `ssr_types::haircut_bps`. Outputs `available
    ///      collateral` haircut-adjusted across the supplied mints.
    ///   2. **Liabilities (Phase 4 v1a).** A `getProgramAccounts`
    ///      memcmp pair on `ssr-lending` and `ssr-repo` enumerates the
    ///      user's open loans/repos as borrower and as lender. Loan
    ///      liability is `principal + accrued simple interest` (one
    ///      `getSlot` projects interest at the same slot
    ///      `repay_loan` would see); repo liability is the cash leg
    ///      owed back at close. Both are aggregated per cash mint and
    ///      subtracted from `available collateral` to print `net
    ///      margin`.
    ///
    /// No FX across cash mints (no oracle in v1a) — net margin is
    /// reported per cash mint. The `ssr-lending` / `ssr-repo` program
    /// IDs are optional; the view degrades gracefully when one isn't
    /// configured.
    Show {
        /// User pubkey whose positions to walk.
        #[arg(long)]
        user: String,
        /// Mint to include in the view. Repeat for each mint
        /// (`--mint A --mint B --mint C`). Mints where the user has
        /// no Position PDA are silently skipped. Cash mints referenced
        /// by the user's open loans/repos but absent here are flagged
        /// in the output — their holdings are excluded from `available
        /// collateral` until the operator adds them to `--mint`.
        #[arg(long = "mint", value_name = "PUBKEY", required = true, num_args = 1..)]
        mints: Vec<String>,
    },
}

#[derive(Subcommand)]
enum DeriveCmd {
    /// Derive a participant's AccountRecord PDA.
    Record {
        #[arg(long)]
        participant: String,
    },
    /// Derive the global Registry PDA.
    Registry,
    /// Derive a vault PDA from a mint.
    Vault {
        #[arg(long)]
        mint: String,
    },
    /// Derive a depositor's Position PDA.
    Position {
        #[arg(long)]
        mint: String,
        #[arg(long)]
        depositor: String,
    },
    /// Derive a Repo PDA.
    Repo {
        #[arg(long)]
        borrower: String,
        #[arg(long)]
        lender: String,
        #[arg(long)]
        collateral_mint: String,
        #[arg(long)]
        cash_mint: String,
        #[arg(long)]
        nonce: u64,
    },
    /// Derive a Loan PDA.
    Loan {
        #[arg(long)]
        borrower: String,
        #[arg(long)]
        lender: String,
        #[arg(long)]
        collateral_mint: String,
        #[arg(long)]
        cash_mint: String,
        #[arg(long)]
        nonce: u64,
    },
    /// Derive a canonical Token-2022 associated-token-account address.
    Ata {
        #[arg(long)]
        owner: String,
        #[arg(long)]
        mint: String,
    },
    /// Derive the SPC SwapDvp PDA for a 6-tuple of (authority, a, b, mint_a, mint_b, nonce).
    SwapDvp {
        #[arg(long)]
        settlement_authority: String,
        #[arg(long)]
        user_a: String,
        #[arg(long)]
        user_b: String,
        #[arg(long)]
        mint_a: String,
        #[arg(long)]
        mint_b: String,
        #[arg(long)]
        nonce: u64,
    },
}

// ─── Solana CLI config loader ──────────────────────────────────────────-

#[derive(Deserialize)]
struct SolanaCliConfig {
    json_rpc_url: Option<String>,
    keypair_path: Option<String>,
}

fn load_solana_cli_config() -> Option<SolanaCliConfig> {
    let path = shellexpand::tilde("~/.config/solana/cli/config.yml").into_owned();
    let bytes = std::fs::read(path).ok()?;
    serde_yaml::from_slice::<SolanaCliConfig>(&bytes).ok()
}

// ─── Effective config (CLI flags → env → solana config → defaults) ─────-

/// Lazy holder for the five SSR program IDs. Each one is parsed at
/// startup (so an invalid base58 surfaces immediately), but the
/// per-program accessor only errors with a "missing X" message when
/// the running command actually needs that ID. A `vault deposit` does
/// not require `SSR_WRAPPER_PROGRAM`; a `compliance verify` does not
/// require any of the wrapper / vault / repo / lending IDs.
struct ProgramIds {
    compliance_opt: Option<Address>,
    wrapper_opt: Option<Address>,
    vault_opt: Option<Address>,
    repo_opt: Option<Address>,
    lending_opt: Option<Address>,
}

impl ProgramIds {
    fn from_cli(cli: &Cli) -> Result<Self> {
        Ok(Self {
            compliance_opt: parse_optional_program(&cli.compliance_program, "compliance")?,
            wrapper_opt: parse_optional_program(&cli.wrapper_program, "wrapper")?,
            vault_opt: parse_optional_program(&cli.vault_program, "vault")?,
            repo_opt: parse_optional_program(&cli.repo_program, "repo")?,
            lending_opt: parse_optional_program(&cli.lending_program, "lending")?,
        })
    }
    fn compliance(&self) -> Result<Address> {
        self.compliance_opt
            .ok_or_else(|| anyhow!("missing --compliance-program / SSR_COMPLIANCE_PROGRAM"))
    }
    fn wrapper(&self) -> Result<Address> {
        self.wrapper_opt
            .ok_or_else(|| anyhow!("missing --wrapper-program / SSR_WRAPPER_PROGRAM"))
    }
    fn vault(&self) -> Result<Address> {
        self.vault_opt
            .ok_or_else(|| anyhow!("missing --vault-program / SSR_VAULT_PROGRAM"))
    }
    fn repo(&self) -> Result<Address> {
        self.repo_opt
            .ok_or_else(|| anyhow!("missing --repo-program / SSR_REPO_PROGRAM"))
    }
    fn lending(&self) -> Result<Address> {
        self.lending_opt
            .ok_or_else(|| anyhow!("missing --lending-program / SSR_LENDING_PROGRAM"))
    }
}

fn parse_optional_program(raw: &Option<String>, name: &str) -> Result<Option<Address>> {
    raw.as_ref()
        .map(|s| Address::from_str(s).with_context(|| format!("invalid {name} program pubkey")))
        .transpose()
}

struct Ctx {
    rpc: RpcClient,
    /// Loaded at startup if a path resolves (so a bad path fails fast),
    /// otherwise `None`. Read-only commands (`compliance status`,
    /// `vault state`, `repo state`, `lending state`, `dvp
    /// authority-address`) never call `signer()` and so don't require
    /// a keypair to be configured.
    keypair_opt: Option<Keypair>,
    programs: ProgramIds,
}

const DEFAULT_RPC: &str = "http://127.0.0.1:8899";

impl Ctx {
    fn from_cli(cli: &Cli) -> Result<Self> {
        let solana_config = load_solana_cli_config();
        let rpc_url = cli
            .rpc_url
            .clone()
            .or_else(|| solana_config.as_ref().and_then(|c| c.json_rpc_url.clone()))
            .unwrap_or_else(|| DEFAULT_RPC.to_string());
        let keypair_path = cli
            .keypair
            .clone()
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| solana_config.as_ref().and_then(|c| c.keypair_path.clone()));
        let keypair_opt = match keypair_path {
            Some(p) => Some(
                read_keypair_file(shellexpand::tilde(&p).as_ref())
                    .map_err(|e| anyhow!("failed to read keypair {p}: {e}"))?,
            ),
            None => None,
        };
        Ok(Self {
            rpc: RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed()),
            keypair_opt,
            programs: ProgramIds::from_cli(cli)?,
        })
    }

    /// Borrow the loaded signer, or error with the same message
    /// `Ctx::from_cli` used to produce when keypair resolution failed.
    fn signer(&self) -> Result<&Keypair> {
        self.keypair_opt.as_ref().ok_or_else(|| {
            anyhow!(
                "no keypair provided; pass --keypair, set SSR_KEYPAIR, \
                 or configure ~/.config/solana/cli/config.yml"
            )
        })
    }

    fn derive_registry(&self) -> Result<Address> {
        Ok(Address::find_program_address(&[seeds::REGISTRY], &self.programs.compliance()?).0)
    }
    fn derive_risk_params(&self) -> Result<Address> {
        Ok(Address::find_program_address(&[seeds::RISK_PARAMS], &self.programs.compliance()?).0)
    }
    fn derive_price_feed(&self, mint: &Address) -> Result<Address> {
        Ok(Address::find_program_address(
            &[seeds::PRICE_FEED, mint.as_ref()],
            &self.programs.compliance()?,
        )
        .0)
    }
    fn derive_pyth_config(&self) -> Result<Address> {
        Ok(Address::find_program_address(&[seeds::PYTH_CONFIG], &self.programs.compliance()?).0)
    }
    fn derive_record(&self, participant: &Address) -> Result<Address> {
        Ok(Address::find_program_address(
            &[seeds::ACCOUNT_RECORD, participant.as_ref()],
            &self.programs.compliance()?,
        )
        .0)
    }
    fn derive_wrapper_authority(&self) -> Result<Address> {
        Ok(Address::find_program_address(&[AUTHORITY_SEED], &self.programs.wrapper()?).0)
    }
    fn derive_vault(&self, mint: &Address) -> Result<Address> {
        Ok(Address::find_program_address(&[seeds::VAULT, mint.as_ref()], &self.programs.vault()?).0)
    }
    fn derive_position(&self, vault: &Address, depositor: &Address) -> Result<Address> {
        Ok(Address::find_program_address(
            &[seeds::POSITION, vault.as_ref(), depositor.as_ref()],
            &self.programs.vault()?,
        )
        .0)
    }
    fn derive_repo(
        &self,
        borrower: &Address,
        lender: &Address,
        collateral_vault: &Address,
        cash_vault: &Address,
        nonce: u64,
    ) -> Result<Address> {
        let nonce_bytes = nonce.to_le_bytes();
        Ok(Address::find_program_address(
            &[
                seeds::REPO,
                borrower.as_ref(),
                lender.as_ref(),
                collateral_vault.as_ref(),
                cash_vault.as_ref(),
                &nonce_bytes,
            ],
            &self.programs.repo()?,
        )
        .0)
    }
    fn derive_loan(
        &self,
        borrower: &Address,
        lender: &Address,
        collateral_vault: &Address,
        cash_vault: &Address,
        nonce: u64,
    ) -> Result<Address> {
        let nonce_bytes = nonce.to_le_bytes();
        Ok(Address::find_program_address(
            &[
                seeds::LOAN,
                borrower.as_ref(),
                lender.as_ref(),
                collateral_vault.as_ref(),
                cash_vault.as_ref(),
                &nonce_bytes,
            ],
            &self.programs.lending()?,
        )
        .0)
    }
    fn derive_loan_list(&self, borrower: &Address) -> Result<Address> {
        Ok(Address::find_program_address(
            &[seeds::LOAN_LIST, borrower.as_ref()],
            &self.programs.lending()?,
        )
        .0)
    }

    fn send(&self, ixs: &[Instruction]) -> Result<()> {
        self.send_with_signers(ixs, &[self.signer()?])
    }

    /// Multi-signer send. The first signer pays the fee. Returns the
    /// confirmed signature (printed to stdout for the user).
    fn send_with_signers(&self, ixs: &[Instruction], signers: &[&Keypair]) -> Result<()> {
        let blockhash = self
            .rpc
            .get_latest_blockhash()
            .context("get latest blockhash")?;
        let payer = signers[0].pubkey();
        let msg = Message::new(ixs, Some(&payer));
        let tx = Transaction::new(signers, msg, blockhash);
        let sig = self
            .rpc
            .send_and_confirm_transaction(&tx)
            .context("send_and_confirm")?;
        println!("✓ tx confirmed: {sig}");
        Ok(())
    }

    /// Read a Token-2022 (or legacy SPL Token) mint's `decimals`
    /// field from offset 44 of the account data.
    fn read_mint_decimals(&self, mint: &Address) -> Result<u8> {
        let acc = self
            .rpc
            .get_account(mint)
            .with_context(|| format!("mint {mint} not found"))?;
        if acc.data.len() < 45 {
            bail!("mint {mint} data too short");
        }
        Ok(acc.data[44])
    }
}

// ─── Instruction builders (mirror programs' on-chain account orderings) ─-

fn ix_initialize_registry(ctx: &Ctx) -> Result<Instruction> {
    Ok(Instruction {
        program_id: ctx.programs.compliance()?,
        accounts: vec![
            AccountMeta::new(ctx.signer()?.pubkey(), true),
            AccountMeta::new(ctx.derive_registry()?, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data: vec![ssr_compliance::ix::INITIALIZE_REGISTRY],
    })
}

fn ix_register_account(
    ctx: &Ctx,
    participant: &Address,
    jurisdiction: [u8; 2],
) -> Result<Instruction> {
    let mut data = Vec::with_capacity(1 + 32 + 2);
    data.push(ssr_compliance::ix::REGISTER_ACCOUNT);
    data.extend_from_slice(participant.as_ref());
    data.extend_from_slice(&jurisdiction);
    let signer = ctx.signer()?.pubkey();
    Ok(Instruction {
        program_id: ctx.programs.compliance()?,
        accounts: vec![
            AccountMeta::new_readonly(signer, true), // operator
            AccountMeta::new(signer, true),          // payer
            AccountMeta::new_readonly(ctx.derive_registry()?, false),
            AccountMeta::new(ctx.derive_record(participant)?, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data,
    })
}

fn ix_update_status(ctx: &Ctx, participant: &Address, status: u8) -> Result<Instruction> {
    Ok(Instruction {
        program_id: ctx.programs.compliance()?,
        accounts: vec![
            AccountMeta::new_readonly(ctx.signer()?.pubkey(), true),
            AccountMeta::new_readonly(ctx.derive_registry()?, false),
            AccountMeta::new(ctx.derive_record(participant)?, false),
        ],
        data: vec![
            ssr_compliance::ix::UPDATE_STATUS,
            status,
            0,
            ssr_compliance::change_mask::STATUS,
        ],
    })
}

fn ix_init_vault(ctx: &Ctx, mint: &Address, asset_class: u8) -> Result<Instruction> {
    Ok(Instruction {
        program_id: ctx.programs.vault()?,
        accounts: vec![
            AccountMeta::new(ctx.signer()?.pubkey(), true),
            AccountMeta::new(ctx.derive_vault(mint)?, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        // Trailing byte: ssr-vault parses 1 byte → UNKNOWN, 2 bytes →
        // explicit class. Always send the byte from this CLI so an
        // operator's `--asset-class` flag round-trips cleanly.
        data: vec![ssr_vault::ix::INIT_VAULT, asset_class],
    })
}

fn ix_init_position(ctx: &Ctx, mint: &Address) -> Result<Instruction> {
    let vault = ctx.derive_vault(mint)?;
    let depositor = ctx.signer()?.pubkey();
    Ok(Instruction {
        program_id: ctx.programs.vault()?,
        accounts: vec![
            AccountMeta::new_readonly(depositor, true),
            AccountMeta::new(depositor, true), // also payer
            AccountMeta::new_readonly(vault, false),
            AccountMeta::new(ctx.derive_position(&vault, &depositor)?, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data: vec![ssr_vault::ix::INIT_POSITION],
    })
}

#[allow(clippy::too_many_arguments)]
fn ix_vault_deposit(
    ctx: &Ctx,
    mint: &Address,
    depositor: &Address,
    depositor_ata: &Address,
    vault: &Address,
    vault_ata: &Address,
    amount: u64,
) -> Result<Instruction> {
    let mut data = Vec::with_capacity(1 + 8);
    data.push(ssr_vault::ix::DEPOSIT);
    data.extend_from_slice(&amount.to_le_bytes());
    Ok(Instruction {
        program_id: ctx.programs.vault()?,
        accounts: vec![
            AccountMeta::new_readonly(*depositor, true),
            AccountMeta::new(ctx.signer()?.pubkey(), true), // payer
            AccountMeta::new_readonly(ctx.derive_record(depositor)?, false),
            AccountMeta::new_readonly(ctx.programs.compliance()?, false),
            AccountMeta::new(*vault, false),
            AccountMeta::new(ctx.derive_position(vault, depositor)?, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*depositor_ata, false),
            AccountMeta::new(*vault_ata, false),
            AccountMeta::new_readonly(ssr_token_2022_id(), false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data,
    })
}

fn ix_vault_withdraw(
    ctx: &Ctx,
    mint: &Address,
    depositor: &Address,
    depositor_ata: &Address,
    vault: &Address,
    vault_ata: &Address,
    amount: u64,
) -> Result<Instruction> {
    let mut data = Vec::with_capacity(1 + 8);
    data.push(ssr_vault::ix::WITHDRAW);
    data.extend_from_slice(&amount.to_le_bytes());
    Ok(Instruction {
        program_id: ctx.programs.vault()?,
        accounts: vec![
            AccountMeta::new_readonly(*depositor, true),
            AccountMeta::new_readonly(ctx.derive_record(depositor)?, false),
            AccountMeta::new_readonly(ctx.programs.compliance()?, false),
            AccountMeta::new(*vault, false),
            AccountMeta::new(ctx.derive_position(vault, depositor)?, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*vault_ata, false),
            AccountMeta::new(*depositor_ata, false),
            AccountMeta::new_readonly(ssr_token_2022_id(), false),
        ],
        data,
    })
}

#[allow(clippy::too_many_arguments)]
fn ix_open_repo(
    ctx: &Ctx,
    borrower: &Address,
    lender: &Address,
    collateral_mint: &Address,
    cash_mint: &Address,
    collateral_amount: u64,
    cash_amount: u64,
    expiry_slot: u64,
    nonce: u64,
) -> Result<Instruction> {
    let collateral_vault = ctx.derive_vault(collateral_mint)?;
    let cash_vault = ctx.derive_vault(cash_mint)?;
    let collateral_position = ctx.derive_position(&collateral_vault, borrower)?;
    let cash_position = ctx.derive_position(&cash_vault, lender)?;
    let repo = ctx.derive_repo(borrower, lender, &collateral_vault, &cash_vault, nonce)?;
    let mut data = Vec::with_capacity(1 + 32);
    data.push(ssr_repo::ix::OPEN_REPO);
    data.extend_from_slice(&collateral_amount.to_le_bytes());
    data.extend_from_slice(&cash_amount.to_le_bytes());
    data.extend_from_slice(&expiry_slot.to_le_bytes());
    data.extend_from_slice(&nonce.to_le_bytes());
    Ok(Instruction {
        program_id: ctx.programs.repo()?,
        accounts: vec![
            AccountMeta::new_readonly(*borrower, true),
            AccountMeta::new_readonly(*lender, true),
            AccountMeta::new(ctx.signer()?.pubkey(), true), // payer
            AccountMeta::new_readonly(ctx.derive_record(borrower)?, false),
            AccountMeta::new_readonly(ctx.derive_record(lender)?, false),
            AccountMeta::new_readonly(ctx.programs.compliance()?, false),
            AccountMeta::new_readonly(ctx.programs.vault()?, false),
            AccountMeta::new_readonly(collateral_vault, false),
            AccountMeta::new(collateral_position, false),
            AccountMeta::new_readonly(cash_vault, false),
            AccountMeta::new(cash_position, false),
            AccountMeta::new(repo, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data,
    })
}

fn ix_close_repo(
    ctx: &Ctx,
    borrower: &Address,
    lender: &Address,
    collateral_mint: &Address,
    cash_mint: &Address,
    nonce: u64,
) -> Result<Instruction> {
    let collateral_vault = ctx.derive_vault(collateral_mint)?;
    let cash_vault = ctx.derive_vault(cash_mint)?;
    let collateral_position = ctx.derive_position(&collateral_vault, borrower)?;
    let cash_position = ctx.derive_position(&cash_vault, lender)?;
    let repo = ctx.derive_repo(borrower, lender, &collateral_vault, &cash_vault, nonce)?;
    Ok(Instruction {
        program_id: ctx.programs.repo()?,
        accounts: vec![
            AccountMeta::new_readonly(*borrower, true),
            AccountMeta::new_readonly(ctx.programs.vault()?, false),
            AccountMeta::new_readonly(collateral_vault, false),
            AccountMeta::new(collateral_position, false),
            AccountMeta::new_readonly(cash_vault, false),
            AccountMeta::new(cash_position, false),
            AccountMeta::new(repo, false),
        ],
        data: vec![ssr_repo::ix::CLOSE_REPO],
    })
}

#[allow(clippy::too_many_arguments)]
fn ix_open_loan(
    ctx: &Ctx,
    borrower: &Address,
    lender: &Address,
    collateral_mint: &Address,
    cash_mint: &Address,
    collateral_amount: u64,
    principal_amount: u64,
    maturity_slot: u64,
    nonce: u64,
    interest_bps_per_year: u32,
) -> Result<Instruction> {
    let collateral_vault = ctx.derive_vault(collateral_mint)?;
    let cash_vault = ctx.derive_vault(cash_mint)?;
    let collateral_position = ctx.derive_position(&collateral_vault, borrower)?;
    let lender_cash_position = ctx.derive_position(&cash_vault, lender)?;
    let borrower_cash_position = ctx.derive_position(&cash_vault, borrower)?;
    let loan = ctx.derive_loan(borrower, lender, &collateral_vault, &cash_vault, nonce)?;
    // Phase 4 v1e: auto-discover per-loan cash mints. For each loan
    // in the borrower's LoanList, read its cash_vault, then read the
    // vault to extract its mint. The distinct cash-mint set becomes
    // the price-feed set (plus collateral + new cash mints).
    if read_risk_params_opt(ctx)?.is_none() {
        bail!(
            "RiskParams PDA not initialized — run `ssr-cli compliance init-risk-params` first."
        );
    }
    let existing_loan_pdas: Vec<Address> = read_loan_list_opt(ctx, borrower)?
        .map(|ll| {
            ll.entries[..ll.count as usize]
                .iter()
                .map(|p| Address::new_from_array(*p))
                .collect()
        })
        .unwrap_or_default();

    // For each existing loan, read it to get its cash_vault, then
    // read the cash_vault to get its mint. Build the parallel
    // cash_vault list (same order as loans) and the deduped
    // cash-mint list for price feeds.
    let mut existing_loan_cash_vaults: Vec<Address> = Vec::with_capacity(existing_loan_pdas.len());
    let mut distinct_cash_mints: std::collections::BTreeSet<[u8; 32]> =
        std::collections::BTreeSet::new();
    for loan_pda in &existing_loan_pdas {
        let l = read_loan_by_pda(ctx, loan_pda)?;
        let cv = Address::new_from_array(l.cash_vault);
        let v = read_vault_by_pda(ctx, &cv)?;
        existing_loan_cash_vaults.push(cv);
        distinct_cash_mints.insert(v.mint);
    }
    // Always include the new loan's collateral and cash mints; the
    // BTreeSet dedupes if the new cash mint also appears in an
    // existing loan.
    distinct_cash_mints.insert(collateral_mint.to_bytes());
    distinct_cash_mints.insert(cash_mint.to_bytes());

    // Verify every required PriceFeed exists; surface a clear
    // migration message if not.
    for mint_bytes in &distinct_cash_mints {
        let mint = Address::new_from_array(*mint_bytes);
        if read_price_feed_opt(ctx, &mint)?.is_none() {
            bail!(
                "PriceFeed for mint {mint} is not registered.\n\
                 Run `ssr-cli compliance register-price-feed --mint {mint} \\\n  \
                 --price-micro-usd <N> --mint-decimals <D>` first."
            );
        }
    }

    // Build the price-feed PDA list, sorted ascending so the
    // handler's strict-ascending check passes.
    let mut feeds: Vec<Address> = distinct_cash_mints
        .iter()
        .map(|m| {
            let mint = Address::new_from_array(*m);
            ctx.derive_price_feed(&mint)
        })
        .collect::<Result<Vec<_>>>()?;
    feeds.sort();

    let mut data = Vec::with_capacity(1 + 39);
    data.push(ssr_lending::ix::OPEN_LOAN);
    data.extend_from_slice(&collateral_amount.to_le_bytes());
    data.extend_from_slice(&principal_amount.to_le_bytes());
    data.extend_from_slice(&maturity_slot.to_le_bytes());
    data.extend_from_slice(&nonce.to_le_bytes());
    data.extend_from_slice(&interest_bps_per_year.to_le_bytes());
    data.push(0u8); // extra_positions_count — no cross-mint position disclosure in MVP
    data.push(feeds.len() as u8);
    data.push(existing_loan_pdas.len() as u8);

    let mut accounts = vec![
        AccountMeta::new_readonly(*borrower, true),
        AccountMeta::new_readonly(*lender, true),
        AccountMeta::new(ctx.signer()?.pubkey(), true), // payer
        AccountMeta::new_readonly(ctx.derive_record(borrower)?, false),
        AccountMeta::new_readonly(ctx.derive_record(lender)?, false),
        AccountMeta::new_readonly(ctx.programs.compliance()?, false),
        AccountMeta::new_readonly(ctx.programs.vault()?, false),
        AccountMeta::new_readonly(collateral_vault, false),
        AccountMeta::new(collateral_position, false),
        AccountMeta::new_readonly(cash_vault, false),
        AccountMeta::new(lender_cash_position, false),
        AccountMeta::new(borrower_cash_position, false),
        AccountMeta::new(loan, false),
        AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        AccountMeta::new(ctx.derive_loan_list(borrower)?, false),
        AccountMeta::new_readonly(ctx.derive_risk_params()?, false),
    ];
    for loan_pda in &existing_loan_pdas {
        accounts.push(AccountMeta::new_readonly(*loan_pda, false));
    }
    for cv in &existing_loan_cash_vaults {
        accounts.push(AccountMeta::new_readonly(*cv, false));
    }
    for feed in &feeds {
        accounts.push(AccountMeta::new_readonly(*feed, false));
    }
    Ok(Instruction {
        program_id: ctx.programs.lending()?,
        accounts,
        data,
    })
}

fn read_loan_list_opt(ctx: &Ctx, borrower: &Address) -> Result<Option<LoanList>> {
    let pda = ctx.derive_loan_list(borrower)?;
    match ctx.rpc.get_account(&pda) {
        Ok(acc) => {
            if acc.data.is_empty() {
                return Ok(None);
            }
            let ll: &LoanList = bytemuck::try_from_bytes(&acc.data[..LoanList::LEN])
                .map_err(|e| anyhow!("loan_list layout invalid: {e:?}"))?;
            Ok(Some(*ll))
        }
        Err(e) => {
            let msg = format!("{e:?}");
            if msg.contains("AccountNotFound") {
                Ok(None)
            } else {
                Err(e).with_context(|| format!("read loan_list PDA {pda}"))
            }
        }
    }
}

fn ix_repay_loan(
    ctx: &Ctx,
    borrower: &Address,
    lender: &Address,
    collateral_mint: &Address,
    cash_mint: &Address,
    nonce: u64,
) -> Result<Instruction> {
    let collateral_vault = ctx.derive_vault(collateral_mint)?;
    let cash_vault = ctx.derive_vault(cash_mint)?;
    let collateral_position = ctx.derive_position(&collateral_vault, borrower)?;
    let borrower_cash_position = ctx.derive_position(&cash_vault, borrower)?;
    let lender_cash_position = ctx.derive_position(&cash_vault, lender)?;
    let loan = ctx.derive_loan(borrower, lender, &collateral_vault, &cash_vault, nonce)?;
    Ok(Instruction {
        program_id: ctx.programs.lending()?,
        accounts: vec![
            AccountMeta::new_readonly(*borrower, true),
            AccountMeta::new_readonly(ctx.programs.vault()?, false),
            AccountMeta::new_readonly(collateral_vault, false),
            AccountMeta::new(collateral_position, false),
            AccountMeta::new_readonly(cash_vault, false),
            AccountMeta::new(borrower_cash_position, false),
            AccountMeta::new(lender_cash_position, false),
            AccountMeta::new(loan, false),
            AccountMeta::new(ctx.derive_loan_list(borrower)?, false),
        ],
        data: vec![ssr_lending::ix::REPAY_LOAN],
    })
}

fn ix_liquidate_loan(
    ctx: &Ctx,
    borrower: &Address,
    lender: &Address,
    collateral_mint: &Address,
    cash_mint: &Address,
    nonce: u64,
) -> Result<Instruction> {
    let collateral_vault = ctx.derive_vault(collateral_mint)?;
    let cash_vault = ctx.derive_vault(cash_mint)?;
    let borrower_collateral = ctx.derive_position(&collateral_vault, borrower)?;
    let lender_collateral = ctx.derive_position(&collateral_vault, lender)?;
    let loan = ctx.derive_loan(borrower, lender, &collateral_vault, &cash_vault, nonce)?;
    Ok(Instruction {
        program_id: ctx.programs.lending()?,
        accounts: vec![
            AccountMeta::new_readonly(*lender, true),
            AccountMeta::new_readonly(ctx.programs.vault()?, false),
            AccountMeta::new_readonly(collateral_vault, false),
            AccountMeta::new(borrower_collateral, false),
            AccountMeta::new(lender_collateral, false),
            AccountMeta::new(loan, false),
            AccountMeta::new(ctx.derive_loan_list(borrower)?, false),
        ],
        data: vec![ssr_lending::ix::LIQUIDATE_LOAN],
    })
}

fn ix_rotate_operators(
    ctx: &Ctx,
    role_byte: u8,
    new_pubkey: &Address,
) -> Result<Instruction> {
    let mut data = Vec::with_capacity(1 + 1 + 32);
    data.push(ssr_compliance::ix::ROTATE_OPERATORS);
    data.push(role_byte);
    data.extend_from_slice(new_pubkey.as_ref());
    Ok(Instruction {
        program_id: ctx.programs.compliance()?,
        accounts: vec![
            AccountMeta::new_readonly(ctx.signer()?.pubkey(), true), // super_admin
            AccountMeta::new(ctx.derive_registry()?, false),
        ],
        data,
    })
}

fn ix_initialize_risk_params(ctx: &Ctx) -> Result<Instruction> {
    let admin = ctx.signer()?.pubkey();
    Ok(Instruction {
        program_id: ctx.programs.compliance()?,
        accounts: vec![
            AccountMeta::new_readonly(admin, true), // super_admin
            AccountMeta::new(admin, true),          // payer (same signer)
            AccountMeta::new_readonly(ctx.derive_registry()?, false),
            AccountMeta::new(ctx.derive_risk_params()?, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data: vec![ssr_compliance::ix::INITIALIZE_RISK_PARAMS],
    })
}

fn ix_register_price_feed(
    ctx: &Ctx,
    mint: &Address,
    price_micro_usd: u64,
    mint_decimals: u8,
) -> Result<Instruction> {
    let admin = ctx.signer()?.pubkey();
    let mut data = Vec::with_capacity(1 + 32 + 8 + 1);
    data.push(ssr_compliance::ix::REGISTER_PRICE_FEED);
    data.extend_from_slice(mint.as_ref());
    data.extend_from_slice(&price_micro_usd.to_le_bytes());
    data.push(mint_decimals);
    Ok(Instruction {
        program_id: ctx.programs.compliance()?,
        accounts: vec![
            AccountMeta::new_readonly(admin, true),
            AccountMeta::new(admin, true),
            AccountMeta::new_readonly(ctx.derive_registry()?, false),
            AccountMeta::new(ctx.derive_price_feed(mint)?, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data,
    })
}

fn ix_update_price(
    ctx: &Ctx,
    mint: &Address,
    price_micro_usd: u64,
) -> Result<Instruction> {
    let mut data = Vec::with_capacity(1 + 8);
    data.push(ssr_compliance::ix::UPDATE_PRICE);
    data.extend_from_slice(&price_micro_usd.to_le_bytes());
    Ok(Instruction {
        program_id: ctx.programs.compliance()?,
        accounts: vec![
            AccountMeta::new_readonly(ctx.signer()?.pubkey(), true),
            AccountMeta::new_readonly(ctx.derive_registry()?, false),
            AccountMeta::new(ctx.derive_price_feed(mint)?, false),
        ],
        data,
    })
}

fn ix_bind_price_feed_to_pyth(
    ctx: &Ctx,
    mint: &Address,
    pyth_source: &Address,
) -> Result<Instruction> {
    let mut data = Vec::with_capacity(1 + 32);
    data.push(ssr_compliance::ix::BIND_PRICE_FEED_TO_PYTH);
    data.extend_from_slice(pyth_source.as_ref());
    let mut accounts = vec![
        AccountMeta::new_readonly(ctx.signer()?.pubkey(), true),
        AccountMeta::new_readonly(ctx.derive_registry()?, false),
        AccountMeta::new(ctx.derive_price_feed(mint)?, false),
    ];
    // v1g: when PythConfig is initialized, attach it + the bound
    // Pyth account so the on-chain handler validates the owner.
    // Skip the attach when unbinding (`pyth_source == [0; 32]`)
    // since no Pyth account exists to validate.
    if pyth_source != &Address::new_from_array([0u8; 32])
        && read_pyth_config_opt(ctx)?.is_some()
    {
        accounts.push(AccountMeta::new_readonly(ctx.derive_pyth_config()?, false));
        accounts.push(AccountMeta::new_readonly(*pyth_source, false));
    }
    Ok(Instruction {
        program_id: ctx.programs.compliance()?,
        accounts,
        data,
    })
}

fn ix_update_price_from_pyth(
    ctx: &Ctx,
    mint: &Address,
    pyth_source: &Address,
) -> Result<Instruction> {
    let mut accounts = vec![
        AccountMeta::new_readonly(ctx.signer()?.pubkey(), true),
        AccountMeta::new_readonly(ctx.derive_registry()?, false),
        AccountMeta::new(ctx.derive_price_feed(mint)?, false),
        AccountMeta::new_readonly(*pyth_source, false),
    ];
    // v1g: opt into owner-validation when PythConfig exists.
    if read_pyth_config_opt(ctx)?.is_some() {
        accounts.push(AccountMeta::new_readonly(ctx.derive_pyth_config()?, false));
    }
    Ok(Instruction {
        program_id: ctx.programs.compliance()?,
        accounts,
        data: vec![ssr_compliance::ix::UPDATE_PRICE_FROM_PYTH],
    })
}

fn ix_initialize_pyth_config(ctx: &Ctx, pyth_program_id: &Address) -> Result<Instruction> {
    let admin = ctx.signer()?.pubkey();
    let mut data = Vec::with_capacity(1 + 32);
    data.push(ssr_compliance::ix::INITIALIZE_PYTH_CONFIG);
    data.extend_from_slice(pyth_program_id.as_ref());
    Ok(Instruction {
        program_id: ctx.programs.compliance()?,
        accounts: vec![
            AccountMeta::new_readonly(admin, true),
            AccountMeta::new(admin, true),
            AccountMeta::new_readonly(ctx.derive_registry()?, false),
            AccountMeta::new(ctx.derive_pyth_config()?, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
        ],
        data,
    })
}

fn ix_set_pyth_program_id(ctx: &Ctx, new_id: &Address) -> Result<Instruction> {
    let mut data = Vec::with_capacity(1 + 32);
    data.push(ssr_compliance::ix::SET_PYTH_PROGRAM_ID);
    data.extend_from_slice(new_id.as_ref());
    Ok(Instruction {
        program_id: ctx.programs.compliance()?,
        accounts: vec![
            AccountMeta::new_readonly(ctx.signer()?.pubkey(), true),
            AccountMeta::new_readonly(ctx.derive_registry()?, false),
            AccountMeta::new(ctx.derive_pyth_config()?, false),
        ],
        data,
    })
}

fn read_pyth_config_opt(ctx: &Ctx) -> Result<Option<PythConfig>> {
    let pda = ctx.derive_pyth_config()?;
    match ctx.rpc.get_account(&pda) {
        Ok(acc) => {
            if acc.data.is_empty() {
                return Ok(None);
            }
            let cfg: &PythConfig = bytemuck::try_from_bytes(&acc.data[..PythConfig::LEN])
                .map_err(|e| anyhow!("pyth_config layout invalid: {e:?}"))?;
            Ok(Some(*cfg))
        }
        Err(e) => {
            let msg = format!("{e:?}");
            if msg.contains("AccountNotFound") {
                Ok(None)
            } else {
                Err(e).with_context(|| format!("read pyth_config PDA {pda}"))
            }
        }
    }
}

fn ix_set_max_staleness(ctx: &Ctx, slots: u64) -> Result<Instruction> {
    let mut data = Vec::with_capacity(1 + 8);
    data.push(ssr_compliance::ix::SET_MAX_STALENESS);
    data.extend_from_slice(&slots.to_le_bytes());
    Ok(Instruction {
        program_id: ctx.programs.compliance()?,
        accounts: vec![
            AccountMeta::new_readonly(ctx.signer()?.pubkey(), true),
            AccountMeta::new_readonly(ctx.derive_registry()?, false),
            AccountMeta::new(ctx.derive_risk_params()?, false),
        ],
        data,
    })
}

fn read_price_feed_opt(ctx: &Ctx, mint: &Address) -> Result<Option<PriceFeed>> {
    let pda = ctx.derive_price_feed(mint)?;
    match ctx.rpc.get_account(&pda) {
        Ok(acc) => {
            if acc.data.is_empty() {
                return Ok(None);
            }
            let pf: &PriceFeed = bytemuck::try_from_bytes(&acc.data[..PriceFeed::LEN])
                .map_err(|e| anyhow!("price_feed layout invalid: {e:?}"))?;
            Ok(Some(*pf))
        }
        Err(e) => {
            let msg = format!("{e:?}");
            if msg.contains("AccountNotFound") {
                Ok(None)
            } else {
                Err(e).with_context(|| format!("read price_feed PDA {pda}"))
            }
        }
    }
}

fn ix_set_haircut(ctx: &Ctx, class: u8, bps: u16) -> Result<Instruction> {
    let mut data = Vec::with_capacity(1 + 1 + 2);
    data.push(ssr_compliance::ix::SET_HAIRCUT);
    data.push(class);
    data.extend_from_slice(&bps.to_le_bytes());
    Ok(Instruction {
        program_id: ctx.programs.compliance()?,
        accounts: vec![
            AccountMeta::new_readonly(ctx.signer()?.pubkey(), true), // super_admin
            AccountMeta::new_readonly(ctx.derive_registry()?, false),
            AccountMeta::new(ctx.derive_risk_params()?, false),
        ],
        data,
    })
}

// ─── Read paths (RPC GETs + Pod decode) ────────────────────────────────-

fn read_registry(ctx: &Ctx) -> Result<Registry> {
    let pda = ctx.derive_registry()?;
    let acc = ctx
        .rpc
        .get_account(&pda)
        .with_context(|| format!("registry PDA {pda} not found"))?;
    let r: &Registry = bytemuck::try_from_bytes(&acc.data[..Registry::LEN])
        .map_err(|e| anyhow!("registry layout invalid: {e:?}"))?;
    Ok(*r)
}

/// Returns `Ok(Some(rp))` if the PDA exists, `Ok(None)` if it
/// hasn't been initialized yet (the caller can fall back to
/// `default_haircut_bps`), or `Err` for any other RPC / layout
/// failure. The PDA-missing case maps to the RPC error string
/// shape `solana-rpc-client` emits for AccountNotFound.
fn read_risk_params_opt(ctx: &Ctx) -> Result<Option<RiskParams>> {
    let pda = ctx.derive_risk_params()?;
    match ctx.rpc.get_account(&pda) {
        Ok(acc) => {
            let rp: &RiskParams = bytemuck::try_from_bytes(&acc.data[..RiskParams::LEN])
                .map_err(|e| anyhow!("risk_params layout invalid: {e:?}"))?;
            Ok(Some(*rp))
        }
        Err(e) => {
            // The non-blocking client surfaces missing accounts as a
            // typed `AccountNotFound`; the blocking client we use
            // collapses it into the error chain. Match on the rendered
            // message — Solana's wire-level "AccountNotFound" survives
            // the wrap. Anything else (auth, network) bubbles up.
            let msg = format!("{e:?}");
            if msg.contains("AccountNotFound") {
                Ok(None)
            } else {
                Err(e).with_context(|| format!("read risk_params PDA {pda}"))
            }
        }
    }
}

fn read_record(ctx: &Ctx, participant: &Address) -> Result<AccountRecord> {
    let pda = ctx.derive_record(participant)?;
    let acc = ctx
        .rpc
        .get_account(&pda)
        .with_context(|| format!("record PDA {pda} not found — register first?"))?;
    let r: &AccountRecord = bytemuck::try_from_bytes(&acc.data[..AccountRecord::LEN])
        .map_err(|e| anyhow!("record layout invalid: {e:?}"))?;
    Ok(*r)
}

fn read_vault(ctx: &Ctx, mint: &Address) -> Result<Vault> {
    let pda = ctx.derive_vault(mint)?;
    let acc = ctx
        .rpc
        .get_account(&pda)
        .with_context(|| format!("vault PDA {pda} not found — init first?"))?;
    let v: &Vault = bytemuck::try_from_bytes(&acc.data[..Vault::LEN])
        .map_err(|e| anyhow!("vault layout invalid: {e:?}"))?;
    Ok(*v)
}

fn read_loan_by_pda(ctx: &Ctx, pda: &Address) -> Result<Loan> {
    let acc = ctx
        .rpc
        .get_account(pda)
        .with_context(|| format!("loan PDA {pda} not found"))?;
    let l: &Loan = bytemuck::try_from_bytes(&acc.data[..Loan::LEN])
        .map_err(|e| anyhow!("loan layout invalid at {pda}: {e:?}"))?;
    Ok(*l)
}

fn read_vault_by_pda(ctx: &Ctx, pda: &Address) -> Result<Vault> {
    let acc = ctx
        .rpc
        .get_account(pda)
        .with_context(|| format!("vault PDA {pda} not found"))?;
    let v: &Vault = bytemuck::try_from_bytes(&acc.data[..Vault::LEN])
        .map_err(|e| anyhow!("vault layout invalid at {pda}: {e:?}"))?;
    Ok(*v)
}

fn read_position(ctx: &Ctx, mint: &Address, depositor: &Address) -> Result<Position> {
    let vault = ctx.derive_vault(mint)?;
    let pda = ctx.derive_position(&vault, depositor)?;
    let acc = ctx
        .rpc
        .get_account(&pda)
        .with_context(|| format!("position PDA {pda} not found"))?;
    let p: &Position = bytemuck::try_from_bytes(&acc.data[..Position::LEN])
        .map_err(|e| anyhow!("position layout invalid: {e:?}"))?;
    Ok(*p)
}

fn read_repo(
    ctx: &Ctx,
    borrower: &Address,
    lender: &Address,
    collateral_mint: &Address,
    cash_mint: &Address,
    nonce: u64,
) -> Result<Repo> {
    let collateral_vault = ctx.derive_vault(collateral_mint)?;
    let cash_vault = ctx.derive_vault(cash_mint)?;
    let pda = ctx.derive_repo(borrower, lender, &collateral_vault, &cash_vault, nonce)?;
    let acc = ctx
        .rpc
        .get_account(&pda)
        .with_context(|| format!("repo PDA {pda} not found"))?;
    let r: &Repo = bytemuck::try_from_bytes(&acc.data[..Repo::LEN])
        .map_err(|e| anyhow!("repo layout invalid: {e:?}"))?;
    Ok(*r)
}

fn read_loan(
    ctx: &Ctx,
    borrower: &Address,
    lender: &Address,
    collateral_mint: &Address,
    cash_mint: &Address,
    nonce: u64,
) -> Result<Loan> {
    let collateral_vault = ctx.derive_vault(collateral_mint)?;
    let cash_vault = ctx.derive_vault(cash_mint)?;
    let pda = ctx.derive_loan(borrower, lender, &collateral_vault, &cash_vault, nonce)?;
    let acc = ctx
        .rpc
        .get_account(&pda)
        .with_context(|| format!("loan PDA {pda} not found"))?;
    let l: &Loan = bytemuck::try_from_bytes(&acc.data[..Loan::LEN])
        .map_err(|e| anyhow!("loan layout invalid: {e:?}"))?;
    Ok(*l)
}

// ─── getProgramAccounts enumeration (Phase 4 v1a margin netting) ────────-
//
// Both `Loan` and `Repo` lay out `last_modified_slot: u64` (offset
// 0..8), `borrower: [u8;32]` (offset 8..40), `lender: [u8;32]` (offset
// 40..72) under `#[repr(C)]`. Any reorder there silently corrupts the
// margin view — pinned by `{repo,loan}_borrower_lender_offsets_are_stable`
// in `crates/ssr-types/src/lib.rs`.
const BORROWER_OFFSET: usize = 8;
const LENDER_OFFSET: usize = 40;

/// `(as_borrower, as_lender)` open loans involving `user`. Two
/// getProgramAccounts hits (one memcmp per role). Closed / liquidated
/// loans are filtered out client-side.
fn enumerate_open_loans(
    ctx: &Ctx,
    user: &Address,
) -> Result<(Vec<(Address, Loan)>, Vec<(Address, Loan)>)> {
    let program = ctx.programs.lending()?;
    Ok((
        fetch_typed(ctx, &program, Loan::LEN, BORROWER_OFFSET, user, decode_loan)?
            .into_iter()
            .filter(|(_, l)| l.status == loan_status::OPEN)
            .collect(),
        fetch_typed(ctx, &program, Loan::LEN, LENDER_OFFSET, user, decode_loan)?
            .into_iter()
            .filter(|(_, l)| l.status == loan_status::OPEN)
            .collect(),
    ))
}

/// `(as_borrower, as_lender)` open repos involving `user`.
fn enumerate_open_repos(
    ctx: &Ctx,
    user: &Address,
) -> Result<(Vec<(Address, Repo)>, Vec<(Address, Repo)>)> {
    let program = ctx.programs.repo()?;
    Ok((
        fetch_typed(ctx, &program, Repo::LEN, BORROWER_OFFSET, user, decode_repo)?
            .into_iter()
            .filter(|(_, r)| r.status == repo_status::OPEN)
            .collect(),
        fetch_typed(ctx, &program, Repo::LEN, LENDER_OFFSET, user, decode_repo)?
            .into_iter()
            .filter(|(_, r)| r.status == repo_status::OPEN)
            .collect(),
    ))
}

fn decode_loan(data: &[u8]) -> Result<Loan> {
    bytemuck::try_from_bytes::<Loan>(&data[..Loan::LEN])
        .copied()
        .map_err(|e| anyhow!("loan layout invalid: {e:?}"))
}

fn decode_repo(data: &[u8]) -> Result<Repo> {
    bytemuck::try_from_bytes::<Repo>(&data[..Repo::LEN])
        .copied()
        .map_err(|e| anyhow!("repo layout invalid: {e:?}"))
}

fn fetch_typed<T>(
    ctx: &Ctx,
    program: &Address,
    expected_size: usize,
    memcmp_offset: usize,
    pubkey: &Address,
    decode: fn(&[u8]) -> Result<T>,
) -> Result<Vec<(Address, T)>> {
    let config = RpcProgramAccountsConfig {
        filters: Some(vec![
            RpcFilterType::DataSize(expected_size as u64),
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                memcmp_offset,
                pubkey.as_ref().to_vec(),
            )),
        ]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            ..Default::default()
        },
        ..Default::default()
    };
    // `get_program_accounts_with_config` is marked deprecated in
    // favor of a UiAccount-returning variant; we keep using it because
    // it hands back `Account` bytes directly, which is exactly what
    // bytemuck wants. If/when it's removed we'll switch and add a
    // UiAccount decode step.
    #[allow(deprecated)]
    let raw = ctx
        .rpc
        .get_program_accounts_with_config(program, config)
        .with_context(|| {
            format!("getProgramAccounts({program}, dataSize={expected_size}, memcmp@{memcmp_offset})")
        })?;
    raw.into_iter()
        .map(|(k, a)| decode(&a.data).map(|t| (k, t)))
        .collect()
}

fn pretty_repo_status(s: u8) -> &'static str {
    match s {
        repo_status::OPEN => "OPEN",
        repo_status::CLOSED => "CLOSED",
        repo_status::DEFAULTED => "DEFAULTED",
        _ => "<unrecognized>",
    }
}

fn pretty_loan_status(s: u8) -> &'static str {
    match s {
        loan_status::OPEN => "OPEN",
        loan_status::REPAID => "REPAID",
        loan_status::LIQUIDATED => "LIQUIDATED",
        _ => "<unrecognized>",
    }
}

fn pretty_status(status: u8) -> &'static str {
    match status {
        compliance_status::UNKNOWN => "UNKNOWN",
        compliance_status::PENDING => "PENDING",
        compliance_status::VERIFIED => "VERIFIED",
        compliance_status::SUSPENDED => "SUSPENDED",
        compliance_status::BLOCKED => "BLOCKED",
        _ => "<unrecognized>",
    }
}

/// Resolve a human-typed asset-class name (e.g. "stablecoin", "equity")
/// to its `asset_class` discriminant byte. Accepts the canonical
/// uppercase names from `ssr_types::asset_class` (case-insensitive)
/// plus a few common short forms. Errors on unrecognized input so a
/// typo can't silently downgrade a vault to UNKNOWN.
fn parse_asset_class(name: &str) -> Result<u8> {
    let lower = name.to_ascii_lowercase();
    let byte = match lower.as_str() {
        "unknown" => asset_class::UNKNOWN,
        "tokenized_deposit" | "deposit" => asset_class::TOKENIZED_DEPOSIT,
        "stablecoin" | "stable" => asset_class::STABLECOIN,
        "sovereign_bond" | "sovereign" => asset_class::SOVEREIGN_BOND,
        "corporate_bond" | "corporate" => asset_class::CORPORATE_BOND,
        "equity" => asset_class::EQUITY,
        "fund_unit" | "fund" => asset_class::FUND_UNIT,
        "real_asset" | "real" => asset_class::REAL_ASSET,
        "commodity" => asset_class::COMMODITY,
        other => bail!(
            "unknown asset class `{other}` (expected: stablecoin | tokenized_deposit | \
             sovereign_bond | corporate_bond | equity | fund_unit | real_asset | \
             commodity | unknown)"
        ),
    };
    Ok(byte)
}

fn pretty_check_error(e: CheckError) -> &'static str {
    match e {
        CheckError::LayoutInvalid => "layout-invalid",
        CheckError::StatusUnknown => "status-unknown",
        CheckError::Unverified => "unverified",
        CheckError::Suspended => "suspended",
        CheckError::Blocked => "blocked",
    }
}

// ─── DvP / SPC SwapDvp on-chain decode ─────────────────────────────────-

const SWAP_DVP_OFFSET_USER_A: usize = 1;
const SWAP_DVP_OFFSET_USER_B: usize = 33;
const SWAP_DVP_OFFSET_SETTLEMENT_AUTHORITY: usize = 129;
const SWAP_DVP_OFFSET_MINT_A: usize = 65;
const SWAP_DVP_OFFSET_MINT_B: usize = 97;

struct SwapDvpFields {
    user_a: Address,
    user_b: Address,
    mint_a: Address,
    mint_b: Address,
    settlement_authority: Address,
}

fn read_swap_dvp(ctx: &Ctx, swap_dvp: &Address) -> Result<SwapDvpFields> {
    let acc = ctx
        .rpc
        .get_account(swap_dvp)
        .with_context(|| format!("swap_dvp {swap_dvp} not found"))?;
    let spc: Address = SPC_DVP_PROGRAM_ID.into();
    if acc.owner != spc {
        bail!("swap_dvp owner {} is not the SPC dvp-swap-program", acc.owner);
    }
    if acc.data.len() < SWAP_DVP_OFFSET_SETTLEMENT_AUTHORITY + 32 {
        bail!("swap_dvp data too short");
    }
    fn read32(data: &[u8], off: usize) -> Address {
        let mut a = [0u8; 32];
        a.copy_from_slice(&data[off..off + 32]);
        Address::from(a)
    }
    Ok(SwapDvpFields {
        user_a: read32(&acc.data, SWAP_DVP_OFFSET_USER_A),
        user_b: read32(&acc.data, SWAP_DVP_OFFSET_USER_B),
        mint_a: read32(&acc.data, SWAP_DVP_OFFSET_MINT_A),
        mint_b: read32(&acc.data, SWAP_DVP_OFFSET_MINT_B),
        settlement_authority: read32(&acc.data, SWAP_DVP_OFFSET_SETTLEMENT_AUTHORITY),
    })
}

// ─── Command handlers ──────────────────────────────────────────────────-

fn cmd_compliance(ctx: &Ctx, cmd: ComplianceCmd) -> Result<()> {
    match cmd {
        ComplianceCmd::InitRegistry => {
            ctx.send(&[ix_initialize_registry(ctx)?])?;
            let r = read_registry(ctx)?;
            println!(
                "registry initialized\n  super_admin       = {}\n  onboard_operator  = {}\n  status_operator   = {}",
                Address::from(r.super_admin),
                Address::from(r.onboard_operator),
                Address::from(r.status_operator),
            );
            Ok(())
        }
        ComplianceCmd::Register {
            participant,
            jurisdiction,
        } => {
            let participant = Address::from_str(&participant).context("participant pubkey")?;
            let j_bytes: [u8; 2] = jurisdiction.as_bytes().try_into().map_err(|_| {
                anyhow!("--jurisdiction must be exactly 2 chars (ISO 3166-1 alpha-2)")
            })?;
            ctx.send(&[ix_register_account(ctx, &participant, j_bytes)?])?;
            cmd_compliance(
                ctx,
                ComplianceCmd::Status {
                    participant: participant.to_string(),
                },
            )
        }
        ComplianceCmd::Verify { participant } => {
            let p = Address::from_str(&participant).context("participant pubkey")?;
            ctx.send(&[ix_update_status(ctx, &p, compliance_status::VERIFIED)?])?;
            cmd_compliance(
                ctx,
                ComplianceCmd::Status {
                    participant: p.to_string(),
                },
            )
        }
        ComplianceCmd::Suspend { participant } => {
            let p = Address::from_str(&participant).context("participant pubkey")?;
            ctx.send(&[ix_update_status(ctx, &p, compliance_status::SUSPENDED)?])?;
            cmd_compliance(
                ctx,
                ComplianceCmd::Status {
                    participant: p.to_string(),
                },
            )
        }
        ComplianceCmd::Block { participant } => {
            let p = Address::from_str(&participant).context("participant pubkey")?;
            ctx.send(&[ix_update_status(ctx, &p, compliance_status::BLOCKED)?])?;
            cmd_compliance(
                ctx,
                ComplianceCmd::Status {
                    participant: p.to_string(),
                },
            )
        }
        ComplianceCmd::Status { participant } => {
            let p = Address::from_str(&participant).context("participant pubkey")?;
            let r = read_record(ctx, &p)?;
            let gate = r.check_transfer_allowed();
            println!("participant {p}");
            println!("  status          = {} (u8 {})", pretty_status(r.status), r.status);
            println!(
                "  jurisdiction    = {}",
                std::str::from_utf8(&r.jurisdiction).unwrap_or("??")
            );
            println!("  flags           = 0b{:08b}", r.flags);
            println!("  updated_at_slot = {}", r.updated_at_slot);
            println!("  bump            = {}", r.bump);
            println!(
                "  transfer gate   = {}",
                match gate {
                    Ok(()) => "✓ verified — transfers allowed".to_string(),
                    Err(e) => format!("✗ {}", pretty_check_error(e)),
                }
            );
            Ok(())
        }
        ComplianceCmd::ShowRegistry => {
            let r = read_registry(ctx)?;
            println!("registry @ {}", ctx.derive_registry()?);
            println!("  super_admin        = {}", Address::from(r.super_admin));
            println!(
                "  onboard_operator   = {}",
                Address::from(r.onboard_operator)
            );
            println!(
                "  status_operator    = {}",
                Address::from(r.status_operator)
            );
            println!("  version            = {}", r.version);
            println!("  last_modified_slot = {}", r.last_modified_slot);
            Ok(())
        }
        ComplianceCmd::RotateOperator { role, new_pubkey } => {
            let role_byte = match role.as_str() {
                "onboard" => role::ONBOARD,
                "status" => role::STATUS,
                other => bail!("unknown role `{other}` (expected: onboard | status)"),
            };
            let new = Address::from_str(&new_pubkey).context("new_pubkey")?;
            ctx.send(&[ix_rotate_operators(ctx, role_byte, &new)?])?;
            cmd_compliance(ctx, ComplianceCmd::ShowRegistry)
        }
        ComplianceCmd::InitRiskParams => {
            ctx.send(&[ix_initialize_risk_params(ctx)?])?;
            cmd_compliance(ctx, ComplianceCmd::ShowRiskParams)
        }
        ComplianceCmd::SetHaircut { class, bps } => {
            let class_byte = parse_asset_class(&class)?;
            if bps > 10_000 {
                bail!("--bps must be 0..=10_000 (a haircut over 100% has no meaning)");
            }
            ctx.send(&[ix_set_haircut(ctx, class_byte, bps)?])?;
            cmd_compliance(ctx, ComplianceCmd::ShowRiskParams)
        }
        ComplianceCmd::ShowRiskParams => {
            match read_risk_params_opt(ctx)? {
                None => {
                    println!("risk_params @ {} : NOT INITIALIZED", ctx.derive_risk_params()?);
                    println!(
                        "  `ssr-cli margin show` is using ssr_types::default_haircut_bps as fallback."
                    );
                    println!(
                        "  Run `ssr-cli compliance init-risk-params` to allocate the PDA so the haircut table becomes governance-mutable."
                    );
                }
                Some(rp) => {
                    println!("risk_params @ {}", ctx.derive_risk_params()?);
                    println!("  version            = {}", rp.version);
                    println!("  bump               = {}", rp.bump);
                    println!("  last_modified_slot = {}", rp.last_modified_slot);
                    println!("  haircut_bps:");
                    for c in 0u8..(RiskParams::HAIRCUT_TABLE_LEN as u8) {
                        let bps = rp.haircut_for(c);
                        // Suppress the long tail of unused 10_000s
                        // unless they've been touched away from the
                        // init default. Cuts noise on a vanilla view
                        // while still surfacing any operator-set value.
                        let default = default_haircut_bps(c);
                        if bps == 10_000 && default == 10_000 {
                            continue;
                        }
                        let tag = if bps == default {
                            "(default)".to_string()
                        } else {
                            format!("(was default {default})")
                        };
                        println!(
                            "    [{c:>3}] {:<20} = {bps:>5} bps  {tag}",
                            asset_class_label(c),
                        );
                    }
                    println!("  max_staleness_slots = {} (0 = staleness gate disabled)",
                        rp.max_staleness_slots);
                }
            }
            Ok(())
        }
        ComplianceCmd::RegisterPriceFeed { mint, price_micro_usd, mint_decimals } => {
            let mint = Address::from_str(&mint).context("mint pubkey")?;
            ctx.send(&[ix_register_price_feed(ctx, &mint, price_micro_usd, mint_decimals)?])?;
            cmd_compliance(ctx, ComplianceCmd::ShowPriceFeed { mint: mint.to_string() })
        }
        ComplianceCmd::UpdatePrice { mint, price_micro_usd } => {
            let mint = Address::from_str(&mint).context("mint pubkey")?;
            ctx.send(&[ix_update_price(ctx, &mint, price_micro_usd)?])?;
            cmd_compliance(ctx, ComplianceCmd::ShowPriceFeed { mint: mint.to_string() })
        }
        ComplianceCmd::ShowPriceFeed { mint } => {
            let mint = Address::from_str(&mint).context("mint pubkey")?;
            match read_price_feed_opt(ctx, &mint)? {
                None => {
                    println!(
                        "price_feed for mint {mint} @ {} : NOT REGISTERED",
                        ctx.derive_price_feed(&mint)?,
                    );
                    println!(
                        "  Run `ssr-cli compliance register-price-feed --mint {mint} --price-micro-usd <N> --mint-decimals <D>` first."
                    );
                }
                Some(pf) => {
                    println!("price_feed @ {}", ctx.derive_price_feed(&mint)?);
                    println!("  mint                = {}", Address::new_from_array(pf.mint));
                    println!("  price_micro_usd     = {} ($ {:.6})",
                        pf.price_micro_usd,
                        (pf.price_micro_usd as f64) / 1_000_000.0,
                    );
                    println!("  mint_decimals       = {}", pf.mint_decimals);
                    println!("  last_updated_slot   = {}", pf.last_updated_slot);
                    println!("  version             = {}", pf.version);
                    println!("  bump                = {}", pf.bump);
                    if pf.is_pyth_bound() {
                        println!(
                            "  pyth_source         = {} (Pyth-bound)",
                            Address::new_from_array(pf.pyth_source),
                        );
                    } else {
                        println!(
                            "  pyth_source         = (unbound — manual `update-price` only)"
                        );
                    }
                }
            }
            Ok(())
        }
        ComplianceCmd::SetMaxStaleness { slots } => {
            ctx.send(&[ix_set_max_staleness(ctx, slots)?])?;
            cmd_compliance(ctx, ComplianceCmd::ShowRiskParams)
        }
        ComplianceCmd::BindPriceFeedToPyth { mint, pyth_source } => {
            let mint = Address::from_str(&mint).context("mint pubkey")?;
            let pyth = Address::from_str(&pyth_source).context("pyth_source pubkey")?;
            ctx.send(&[ix_bind_price_feed_to_pyth(ctx, &mint, &pyth)?])?;
            cmd_compliance(ctx, ComplianceCmd::ShowPriceFeed { mint: mint.to_string() })
        }
        ComplianceCmd::UpdatePriceFromPyth { mint } => {
            let mint = Address::from_str(&mint).context("mint pubkey")?;
            // The bound Pyth source lives in the PriceFeed itself —
            // read it so the CLI doesn't have to ask the operator
            // to re-supply it. Surfaces a clear error if unbound.
            let pf = read_price_feed_opt(ctx, &mint)?.ok_or_else(|| {
                anyhow!(
                    "PriceFeed for mint {mint} not registered — \
                     run `ssr-cli compliance register-price-feed` first."
                )
            })?;
            if !pf.is_pyth_bound() {
                bail!(
                    "PriceFeed for mint {mint} is not Pyth-bound — \
                     run `ssr-cli compliance bind-price-feed-to-pyth --mint {mint} --pyth-source <PYTH_PDA>` first."
                );
            }
            let pyth = Address::new_from_array(pf.pyth_source);
            ctx.send(&[ix_update_price_from_pyth(ctx, &mint, &pyth)?])?;
            cmd_compliance(ctx, ComplianceCmd::ShowPriceFeed { mint: mint.to_string() })
        }
        ComplianceCmd::InitPythConfig { pyth_program_id } => {
            let pid = Address::from_str(&pyth_program_id).context("pyth_program_id")?;
            ctx.send(&[ix_initialize_pyth_config(ctx, &pid)?])?;
            cmd_compliance(ctx, ComplianceCmd::ShowPythConfig)
        }
        ComplianceCmd::SetPythProgramId { pyth_program_id } => {
            let pid = Address::from_str(&pyth_program_id).context("pyth_program_id")?;
            ctx.send(&[ix_set_pyth_program_id(ctx, &pid)?])?;
            cmd_compliance(ctx, ComplianceCmd::ShowPythConfig)
        }
        ComplianceCmd::ShowPythConfig => {
            match read_pyth_config_opt(ctx)? {
                None => {
                    println!(
                        "pyth_config @ {} : NOT INITIALIZED",
                        ctx.derive_pyth_config()?,
                    );
                    println!(
                        "  Owner-validation on `bind-price-feed-to-pyth` /"
                    );
                    println!(
                        "  `update-price-from-pyth` is disabled (v1f compat)."
                    );
                    println!(
                        "  Run `ssr-cli compliance init-pyth-config --pyth-program-id <PID>` to enable."
                    );
                }
                Some(cfg) => {
                    println!("pyth_config @ {}", ctx.derive_pyth_config()?);
                    println!(
                        "  pyth_program_id     = {}",
                        Address::new_from_array(cfg.pyth_program_id),
                    );
                    println!("  last_modified_slot  = {}", cfg.last_modified_slot);
                    println!("  version             = {}", cfg.version);
                    println!("  bump                = {}", cfg.bump);
                }
            }
            Ok(())
        }
    }
}

fn cmd_vault(ctx: &Ctx, cmd: VaultCmd) -> Result<()> {
    match cmd {
        VaultCmd::Init { mint, asset_class } => {
            let mint = Address::from_str(&mint).context("mint pubkey")?;
            let class_byte = parse_asset_class(&asset_class)?;
            ctx.send(&[ix_init_vault(ctx, &mint, class_byte)?])?;
            cmd_vault(ctx, VaultCmd::State { mint: mint.to_string() })
        }
        VaultCmd::InitPosition { mint } => {
            let mint = Address::from_str(&mint).context("mint pubkey")?;
            ctx.send(&[ix_init_position(ctx, &mint)?])?;
            cmd_vault(
                ctx,
                VaultCmd::Position {
                    mint: mint.to_string(),
                    depositor: Some(ctx.signer()?.pubkey().to_string()),
                },
            )
        }
        VaultCmd::Deposit { mint, amount } => {
            let mint = Address::from_str(&mint).context("mint pubkey")?;
            let depositor = ctx.signer()?.pubkey();
            let vault = ctx.derive_vault(&mint)?;
            let depositor_ata = derive_canonical_ata(
                &depositor,
                &mint,
                &ssr_token_2022_id(),
                &ata_program_id(),
            );
            let vault_ata = derive_canonical_ata(
                &vault,
                &mint,
                &ssr_token_2022_id(),
                &ata_program_id(),
            );
            ctx.send(&[ix_vault_deposit(
                ctx, &mint, &depositor, &depositor_ata, &vault, &vault_ata, amount,
            )?])?;
            cmd_vault(
                ctx,
                VaultCmd::Position {
                    mint: mint.to_string(),
                    depositor: Some(depositor.to_string()),
                },
            )
        }
        VaultCmd::Withdraw { mint, amount } => {
            let mint = Address::from_str(&mint).context("mint pubkey")?;
            let depositor = ctx.signer()?.pubkey();
            let vault = ctx.derive_vault(&mint)?;
            let depositor_ata = derive_canonical_ata(
                &depositor,
                &mint,
                &ssr_token_2022_id(),
                &ata_program_id(),
            );
            let vault_ata = derive_canonical_ata(
                &vault,
                &mint,
                &ssr_token_2022_id(),
                &ata_program_id(),
            );
            let _ = ctx.read_mint_decimals(&mint)?; // sanity: mint exists
            ctx.send(&[ix_vault_withdraw(
                ctx, &mint, &depositor, &depositor_ata, &vault, &vault_ata, amount,
            )?])?;
            cmd_vault(
                ctx,
                VaultCmd::Position {
                    mint: mint.to_string(),
                    depositor: Some(depositor.to_string()),
                },
            )
        }
        VaultCmd::State { mint } => {
            let mint = Address::from_str(&mint).context("mint pubkey")?;
            let v = read_vault(ctx, &mint)?;
            let pda = ctx.derive_vault(&mint)?;
            println!("vault @ {pda}");
            println!("  mint               = {}", Address::from(v.mint));
            println!("  admin              = {}", Address::from(v.admin));
            println!("  total_deposited    = {}", v.total_deposited);
            println!("  position_count     = {}", v.position_count);
            println!(
                "  asset_class        = {} (u8 {}; default haircut {} bps)",
                asset_class_label(v.asset_class),
                v.asset_class,
                default_haircut_bps(v.asset_class),
            );
            println!(
                "  (live haircut shown by `compliance show-risk-params` / used in `margin show`)"
            );
            println!("  version            = {}", v.version);
            println!("  last_modified_slot = {}", v.last_modified_slot);
            Ok(())
        }
        VaultCmd::Position { mint, depositor } => {
            let mint = Address::from_str(&mint).context("mint pubkey")?;
            let depositor = match depositor {
                Some(s) => Address::from_str(&s).context("depositor pubkey")?,
                // Read-only path: fall back to the loaded keypair if
                // present, otherwise require explicit --depositor so
                // we don't pull SSR_KEYPAIR in just to identify whose
                // position to read.
                None => ctx
                    .keypair_opt
                    .as_ref()
                    .map(|k| k.pubkey())
                    .ok_or_else(|| {
                        anyhow!(
                            "no --depositor given and no keypair available; \
                             pass --depositor <pubkey> explicitly"
                        )
                    })?,
            };
            let p = read_position(ctx, &mint, &depositor)?;
            let vault = ctx.derive_vault(&mint)?;
            let pda = ctx.derive_position(&vault, &depositor)?;
            println!("position @ {pda}");
            println!("  vault              = {}", Address::from(p.vault));
            println!("  depositor          = {}", Address::from(p.depositor));
            println!("  amount_deposited   = {}", p.amount_deposited);
            println!("  locked_amount      = {}", p.locked_amount);
            println!("  available          = {}", p.available());
            println!(
                "  lock_authority     = {}",
                if p.is_unlocked() {
                    "(none)".to_string()
                } else {
                    Address::from(p.lock_authority).to_string()
                }
            );
            println!("  last_modified_slot = {}", p.last_modified_slot);
            Ok(())
        }
    }
}

fn cmd_repo(ctx: &Ctx, cmd: RepoCmd) -> Result<()> {
    match cmd {
        RepoCmd::Open {
            borrower_keypair,
            lender_keypair,
            collateral_mint,
            cash_mint,
            collateral_amount,
            cash_amount,
            expiry_slot,
            nonce,
        } => {
            let borrower = read_keypair_file(
                shellexpand::tilde(&borrower_keypair.to_string_lossy()).as_ref(),
            )
            .map_err(|e| anyhow!("read borrower keypair: {e}"))?;
            let lender = read_keypair_file(
                shellexpand::tilde(&lender_keypair.to_string_lossy()).as_ref(),
            )
            .map_err(|e| anyhow!("read lender keypair: {e}"))?;
            let collateral_mint = Address::from_str(&collateral_mint).context("collateral mint")?;
            let cash_mint = Address::from_str(&cash_mint).context("cash mint")?;
            let ix = ix_open_repo(
                ctx,
                &borrower.pubkey(),
                &lender.pubkey(),
                &collateral_mint,
                &cash_mint,
                collateral_amount,
                cash_amount,
                expiry_slot,
                nonce,
            )?;
            // 3 signers: payer (global --keypair) + borrower + lender
            ctx.send_with_signers(&[ix], &[ctx.signer()?, &borrower, &lender])?;
            cmd_repo(
                ctx,
                RepoCmd::State {
                    borrower: borrower.pubkey().to_string(),
                    lender: lender.pubkey().to_string(),
                    collateral_mint: collateral_mint.to_string(),
                    cash_mint: cash_mint.to_string(),
                    nonce,
                },
            )
        }
        RepoCmd::Close {
            lender,
            collateral_mint,
            cash_mint,
            nonce,
        } => {
            let lender = Address::from_str(&lender).context("lender pubkey")?;
            let collateral_mint = Address::from_str(&collateral_mint).context("collateral mint")?;
            let cash_mint = Address::from_str(&cash_mint).context("cash mint")?;
            let borrower = ctx.signer()?.pubkey();
            let ix =
                ix_close_repo(ctx, &borrower, &lender, &collateral_mint, &cash_mint, nonce)?;
            ctx.send(&[ix])?;
            cmd_repo(
                ctx,
                RepoCmd::State {
                    borrower: borrower.to_string(),
                    lender: lender.to_string(),
                    collateral_mint: collateral_mint.to_string(),
                    cash_mint: cash_mint.to_string(),
                    nonce,
                },
            )
        }
        RepoCmd::State {
            borrower,
            lender,
            collateral_mint,
            cash_mint,
            nonce,
        } => {
            let borrower = Address::from_str(&borrower).context("borrower pubkey")?;
            let lender = Address::from_str(&lender).context("lender pubkey")?;
            let collateral_mint = Address::from_str(&collateral_mint).context("collateral mint")?;
            let cash_mint = Address::from_str(&cash_mint).context("cash mint")?;
            let r = read_repo(ctx, &borrower, &lender, &collateral_mint, &cash_mint, nonce)?;
            let collateral_vault = ctx.derive_vault(&collateral_mint)?;
            let cash_vault = ctx.derive_vault(&cash_mint)?;
            let pda = ctx.derive_repo(&borrower, &lender, &collateral_vault, &cash_vault, nonce)?;
            println!("repo @ {pda}");
            println!("  status             = {} (u8 {})", pretty_repo_status(r.status), r.status);
            println!("  borrower           = {}", Address::from(r.borrower));
            println!("  lender             = {}", Address::from(r.lender));
            println!("  collateral_vault   = {}", Address::from(r.collateral_vault));
            println!("  cash_vault         = {}", Address::from(r.cash_vault));
            println!("  collateral_amount  = {}", r.collateral_amount);
            println!("  cash_amount        = {}", r.cash_amount);
            println!("  expiry_slot        = {}", r.expiry_slot);
            println!("  nonce              = {}", r.nonce);
            println!("  last_modified_slot = {}", r.last_modified_slot);
            Ok(())
        }
    }
}

fn cmd_lending(ctx: &Ctx, cmd: LendingCmd) -> Result<()> {
    match cmd {
        LendingCmd::Open {
            borrower_keypair,
            lender_keypair,
            collateral_mint,
            cash_mint,
            collateral_amount,
            principal_amount,
            maturity_slot,
            nonce,
            interest_bps_per_year,
        } => {
            let borrower = read_keypair_file(
                shellexpand::tilde(&borrower_keypair.to_string_lossy()).as_ref(),
            )
            .map_err(|e| anyhow!("read borrower keypair: {e}"))?;
            let lender = read_keypair_file(
                shellexpand::tilde(&lender_keypair.to_string_lossy()).as_ref(),
            )
            .map_err(|e| anyhow!("read lender keypair: {e}"))?;
            let collateral_mint = Address::from_str(&collateral_mint).context("collateral mint")?;
            let cash_mint = Address::from_str(&cash_mint).context("cash mint")?;
            let ix = ix_open_loan(
                ctx,
                &borrower.pubkey(),
                &lender.pubkey(),
                &collateral_mint,
                &cash_mint,
                collateral_amount,
                principal_amount,
                maturity_slot,
                nonce,
                interest_bps_per_year,
            )?;
            // 3 signers: payer (global --keypair) + borrower + lender
            ctx.send_with_signers(&[ix], &[ctx.signer()?, &borrower, &lender])?;
            cmd_lending(
                ctx,
                LendingCmd::State {
                    borrower: borrower.pubkey().to_string(),
                    lender: lender.pubkey().to_string(),
                    collateral_mint: collateral_mint.to_string(),
                    cash_mint: cash_mint.to_string(),
                    nonce,
                },
            )
        }
        LendingCmd::Repay {
            lender,
            collateral_mint,
            cash_mint,
            nonce,
        } => {
            let lender = Address::from_str(&lender).context("lender pubkey")?;
            let collateral_mint = Address::from_str(&collateral_mint).context("collateral mint")?;
            let cash_mint = Address::from_str(&cash_mint).context("cash mint")?;
            let borrower = ctx.signer()?.pubkey();
            let ix =
                ix_repay_loan(ctx, &borrower, &lender, &collateral_mint, &cash_mint, nonce)?;
            ctx.send(&[ix])?;
            cmd_lending(
                ctx,
                LendingCmd::State {
                    borrower: borrower.to_string(),
                    lender: lender.to_string(),
                    collateral_mint: collateral_mint.to_string(),
                    cash_mint: cash_mint.to_string(),
                    nonce,
                },
            )
        }
        LendingCmd::Liquidate {
            borrower,
            collateral_mint,
            cash_mint,
            nonce,
        } => {
            let borrower = Address::from_str(&borrower).context("borrower pubkey")?;
            let collateral_mint =
                Address::from_str(&collateral_mint).context("collateral mint")?;
            let cash_mint = Address::from_str(&cash_mint).context("cash mint")?;
            let lender = ctx.signer()?.pubkey();
            let ix = ix_liquidate_loan(
                ctx,
                &borrower,
                &lender,
                &collateral_mint,
                &cash_mint,
                nonce,
            )?;
            ctx.send(&[ix])?;
            cmd_lending(
                ctx,
                LendingCmd::State {
                    borrower: borrower.to_string(),
                    lender: lender.to_string(),
                    collateral_mint: collateral_mint.to_string(),
                    cash_mint: cash_mint.to_string(),
                    nonce,
                },
            )
        }
        LendingCmd::State {
            borrower,
            lender,
            collateral_mint,
            cash_mint,
            nonce,
        } => {
            let borrower = Address::from_str(&borrower).context("borrower pubkey")?;
            let lender = Address::from_str(&lender).context("lender pubkey")?;
            let collateral_mint = Address::from_str(&collateral_mint).context("collateral mint")?;
            let cash_mint = Address::from_str(&cash_mint).context("cash mint")?;
            let l = read_loan(ctx, &borrower, &lender, &collateral_mint, &cash_mint, nonce)?;
            let collateral_vault = ctx.derive_vault(&collateral_mint)?;
            let cash_vault = ctx.derive_vault(&cash_mint)?;
            let pda = ctx.derive_loan(&borrower, &lender, &collateral_vault, &cash_vault, nonce)?;
            println!("loan @ {pda}");
            println!("  status                = {} (u8 {})", pretty_loan_status(l.status), l.status);
            println!("  borrower              = {}", Address::from(l.borrower));
            println!("  lender                = {}", Address::from(l.lender));
            println!("  collateral_vault      = {}", Address::from(l.collateral_vault));
            println!("  cash_vault            = {}", Address::from(l.cash_vault));
            println!("  collateral_amount     = {}", l.collateral_amount);
            println!("  principal_amount      = {}", l.principal_amount);
            println!("  interest_bps_per_year = {} ({:.2}% / yr)", l.interest_bps_per_year, l.interest_bps_per_year as f64 / 100.0);
            println!("  opened_slot           = {}", l.opened_slot);
            println!("  maturity_slot         = {}", l.maturity_slot);
            println!("  nonce                 = {}", l.nonce);
            println!("  last_modified_slot    = {}", l.last_modified_slot);
            Ok(())
        }
    }
}

fn cmd_dvp(ctx: &Ctx, cmd: DvpCmd) -> Result<()> {
    match cmd {
        DvpCmd::AuthorityAddress => {
            let pda = ctx.derive_wrapper_authority()?;
            println!("{pda}");
            Ok(())
        }
        DvpCmd::Settle {
            swap_dvp,
            leg_a_extras_count,
        } => {
            let swap_dvp = Address::from_str(&swap_dvp).context("swap_dvp pubkey")?;
            let fields = read_swap_dvp(ctx, &swap_dvp)?;
            let wrapper_authority = ctx.derive_wrapper_authority()?;
            if fields.settlement_authority != wrapper_authority {
                bail!(
                    "swap_dvp.settlement_authority = {} but our wrapper PDA = {} — \
                     refusing to attempt a settle that the SPC program will reject",
                    fields.settlement_authority,
                    wrapper_authority
                );
            }
            // Derive ATAs (canonical, Token-2022).
            let token_2022_id: Address = ssr_token_2022_id();
            let ata_program = ata_program_id();
            let dvp_ata_a = derive_canonical_ata(&swap_dvp, &fields.mint_a, &token_2022_id, &ata_program);
            let dvp_ata_b = derive_canonical_ata(&swap_dvp, &fields.mint_b, &token_2022_id, &ata_program);
            let user_a_ata_b =
                derive_canonical_ata(&fields.user_a, &fields.mint_b, &token_2022_id, &ata_program);
            let user_b_ata_a =
                derive_canonical_ata(&fields.user_b, &fields.mint_a, &token_2022_id, &ata_program);
            let user_a_ata_a =
                derive_canonical_ata(&fields.user_a, &fields.mint_a, &token_2022_id, &ata_program);
            let user_b_ata_b =
                derive_canonical_ata(&fields.user_b, &fields.mint_b, &token_2022_id, &ata_program);
            let user_a_record = ctx.derive_record(&fields.user_a)?;
            let user_b_record = ctx.derive_record(&fields.user_b)?;

            let ix = Instruction {
                program_id: ctx.programs.wrapper()?,
                accounts: vec![
                    AccountMeta::new(wrapper_authority, false),
                    AccountMeta::new_readonly(ctx.programs.compliance()?, false),
                    AccountMeta::new_readonly(Address::from(SPC_DVP_PROGRAM_ID), false),
                    AccountMeta::new_readonly(user_a_record, false),
                    AccountMeta::new_readonly(user_b_record, false),
                    AccountMeta::new(swap_dvp, false),
                    AccountMeta::new_readonly(fields.mint_a, false),
                    AccountMeta::new_readonly(fields.mint_b, false),
                    AccountMeta::new(dvp_ata_a, false),
                    AccountMeta::new(dvp_ata_b, false),
                    AccountMeta::new(user_a_ata_b, false),
                    AccountMeta::new(user_b_ata_a, false),
                    AccountMeta::new(user_a_ata_a, false),
                    AccountMeta::new(user_b_ata_b, false),
                    AccountMeta::new_readonly(token_2022_id, false),
                    AccountMeta::new_readonly(token_2022_id, false),
                ],
                data: vec![
                    ssr_dvp_wrapper::ix::COMPLIANT_SETTLE_DVP,
                    leg_a_extras_count,
                ],
            };
            ctx.send(&[ix])
        }
    }
}

fn cmd_margin(ctx: &Ctx, cmd: MarginCmd) -> Result<()> {
    match cmd {
        MarginCmd::Show { user, mints } => {
            let user = Address::from_str(&user).context("user pubkey")?;
            // Mint parse errors are eager (a typo would otherwise be
            // silently dropped under "no position" later).
            let mints: Vec<Address> = mints
                .iter()
                .map(|s| Address::from_str(s).with_context(|| format!("mint pubkey `{s}`")))
                .collect::<Result<_>>()?;

            println!("margin @ {user}\n");

            // Phase 4 v1c: read the governance-mutable haircut table
            // once at the start. If the PDA isn't allocated yet, fall
            // back to `default_haircut_bps` so pre-v1c deployments and
            // lending-only demos still produce a meaningful view. The
            // banner makes the fallback explicit so an operator never
            // mistakes a stale read for the live policy.
            let risk_params = read_risk_params_opt(ctx)?;
            match &risk_params {
                None => {
                    println!(
                        "(risk_params PDA not initialized — falling back to ssr_types::default_haircut_bps; run `ssr-cli compliance init-risk-params` to migrate)\n"
                    );
                }
                Some(rp) => {
                    println!(
                        "(risk_params @ {}, last_modified_slot {})\n",
                        ctx.derive_risk_params()?,
                        rp.last_modified_slot,
                    );
                }
            }
            let haircut_for = |class: u8| -> u16 {
                match &risk_params {
                    Some(rp) => rp.haircut_for(class),
                    None => default_haircut_bps(class),
                }
            };

            // Per-mint accumulators stored in basis-point units to
            // dodge intermediate truncation; one final divide at the
            // end. (deposited * (10_000 - haircut)) sums fit in u128
            // for any sane portfolio.
            let mut sum_gross_bps: u128 = 0;
            let mut sum_locked_bps: u128 = 0;
            let mut sum_avail_bps: u128 = 0;
            let mut covered = 0usize;
            // The set of mints whose vaults we've walked, used below
            // to flag loan/repo cash legs the caller didn't include.
            let mut walked_mints: std::collections::BTreeSet<[u8; 32]> =
                std::collections::BTreeSet::new();

            for mint in &mints {
                let v = read_vault(ctx, mint)?;
                walked_mints.insert(v.mint);
                // A missing position is the "this user never deposited
                // this asset" case — note and move on.
                let vault_pda = ctx.derive_vault(mint)?;
                let pda = ctx.derive_position(&vault_pda, &user)?;
                let position_acc = ctx.rpc.get_account(&pda).ok();
                let p_opt: Option<Position> = position_acc.and_then(|acc| {
                    bytemuck::try_from_bytes::<Position>(&acc.data[..Position::LEN])
                        .ok()
                        .copied()
                });

                let class = v.asset_class;
                let hc_bps = haircut_for(class) as u128;
                let credit_bps = 10_000u128.saturating_sub(hc_bps);

                println!(
                    "  mint {} ({}, haircut {} bps)",
                    mint,
                    asset_class_label(class),
                    hc_bps,
                );
                match p_opt {
                    None => {
                        println!("    (no position — skipped)");
                    }
                    Some(p) => {
                        let deposited = p.amount_deposited as u128;
                        let locked = p.locked_amount as u128;
                        let avail = p.available() as u128;
                        let gross_contrib = deposited * credit_bps / 10_000;
                        let locked_contrib = locked * credit_bps / 10_000;
                        let avail_contrib = avail * credit_bps / 10_000;
                        println!("    deposited            = {}", p.amount_deposited);
                        println!("    locked               = {}", p.locked_amount);
                        println!("    available            = {}", p.available());
                        println!("    haircut-adjusted     = {} (free) / {} (encumbered) / {} (gross)",
                            avail_contrib, locked_contrib, gross_contrib);
                        sum_gross_bps += deposited * credit_bps;
                        sum_locked_bps += locked * credit_bps;
                        sum_avail_bps += avail * credit_bps;
                        covered += 1;
                    }
                }
                println!();
            }

            // ─── Loan-side netting (Phase 4 v1a) ─────────────────────-
            // Two getProgramAccounts hits per program (borrower / lender
            // memcmp) discover the user's encumbrances. Either side is
            // optional: a lending-only or repo-only demo skips the
            // unconfigured program with an inline note rather than
            // failing the whole view.
            let (loans_b, loans_l) = match enumerate_open_loans(ctx, &user) {
                Ok(x) => x,
                Err(e) => {
                    println!("  (skipping loans — {e})\n");
                    (Vec::new(), Vec::new())
                }
            };
            let (repos_b, repos_l) = match enumerate_open_repos(ctx, &user) {
                Ok(x) => x,
                Err(e) => {
                    println!("  (skipping repos — {e})\n");
                    (Vec::new(), Vec::new())
                }
            };

            // One getAccount per distinct cash vault to resolve its
            // mint (for display + cash-mint grouping). Cache so the
            // same vault appearing in N loans/repos costs one RPC.
            let mut vault_cache: std::collections::BTreeMap<Address, Vault> =
                std::collections::BTreeMap::new();
            // Liabilities aren't FX-converted across cash mints (no
            // oracle in v1a); we sum within each mint and present the
            // breakdown so the operator can read it the way an OMS
            // would — per currency.
            let mut liability_by_cash_mint: std::collections::BTreeMap<[u8; 32], u128> =
                std::collections::BTreeMap::new();

            // One `getSlot` if we have at least one loan to project
            // interest for. Repos have no interest concept (just a
            // cash leg owed back), so they don't trigger the lookup.
            let current_slot = if loans_b.is_empty() && loans_l.is_empty() {
                None
            } else {
                Some(ctx.rpc.get_slot().context("getSlot for interest projection")?)
            };

            let print_line = |kind: &str,
                              pda: &Address,
                              amount: u64,
                              cash_mint: &Address,
                              note: &str| {
                println!("  {pda}  {kind}={amount} cash_mint={cash_mint}  {note}");
            };

            if !loans_b.is_empty() {
                println!("open loans (borrowed → liabilities):");
                for (pda, l) in &loans_b {
                    let cash_v = cached_vault(ctx, &mut vault_cache, l.cash_vault)?;
                    let cash_mint = Address::new_from_array(cash_v.mint);
                    let interest = project_loan_interest(l, current_slot.unwrap())?;
                    let total = (l.principal_amount as u128) + interest;
                    println!(
                        "  {pda}  principal={} + interest={interest} = {total}  cash_mint={cash_mint}  → liability",
                        l.principal_amount,
                    );
                    *liability_by_cash_mint
                        .entry(cash_v.mint)
                        .or_insert(0) += total;
                }
                println!();
            }
            if !loans_l.is_empty() {
                println!("open loans (extended, as lender):");
                for (pda, l) in &loans_l {
                    let cash_v = cached_vault(ctx, &mut vault_cache, l.cash_vault)?;
                    let cash_mint = Address::new_from_array(cash_v.mint);
                    let interest = project_loan_interest(l, current_slot.unwrap())?;
                    println!(
                        "  {pda}  principal={} cash_mint={cash_mint}  (principal already locked in cash position; +{interest} interest receivable at repay)",
                        l.principal_amount,
                    );
                }
                println!();
            }
            if !repos_b.is_empty() {
                println!("open repos (borrower side → cash leg owed back):");
                for (pda, r) in &repos_b {
                    let cash_v = cached_vault(ctx, &mut vault_cache, r.cash_vault)?;
                    let cash_mint = Address::new_from_array(cash_v.mint);
                    print_line(
                        "cash_amount",
                        pda,
                        r.cash_amount,
                        &cash_mint,
                        "→ liability (cash flow is off-chain in Phase 3 minimum)",
                    );
                    *liability_by_cash_mint
                        .entry(cash_v.mint)
                        .or_insert(0) += r.cash_amount as u128;
                }
                println!();
            }
            if !repos_l.is_empty() {
                println!("open repos (lender side):");
                for (pda, r) in &repos_l {
                    let cash_v = cached_vault(ctx, &mut vault_cache, r.cash_vault)?;
                    let cash_mint = Address::new_from_array(cash_v.mint);
                    print_line(
                        "cash_amount",
                        pda,
                        r.cash_amount,
                        &cash_mint,
                        "(already counted in your cash position's locked amount)",
                    );
                }
                println!();
            }

            // Warn when liabilities reference a cash mint the caller
            // didn't include — their drawdown / locked cash position
            // isn't reflected in `available collateral`, so net margin
            // will look worse than it is.
            let missing: Vec<[u8; 32]> = liability_by_cash_mint
                .keys()
                .copied()
                .filter(|m| !walked_mints.contains(m))
                .collect();
            if !missing.is_empty() {
                println!(
                    "warning: cash mint(s) in your loans/repos not passed via --mint;"
                );
                println!(
                    "         your holdings in them are excluded from `available collateral`:"
                );
                for m in &missing {
                    println!("           {}", Address::new_from_array(*m));
                }
                println!();
            }

            println!("totals across {covered} positions (haircut-adjusted):");
            println!("  gross collateral     = {}", sum_gross_bps / 10_000);
            println!("  encumbered           = {}", sum_locked_bps / 10_000);
            println!("  available collateral = {}", sum_avail_bps / 10_000);

            if liability_by_cash_mint.is_empty() {
                println!("  liabilities          = (none)");
                println!("  net margin           = {}", sum_avail_bps / 10_000);
            } else {
                println!("liabilities (cash mints the user owes back):");
                for (mint, amt) in &liability_by_cash_mint {
                    println!("  {}: {amt}", Address::new_from_array(*mint));
                }
                println!("net margin per cash mint (no FX across mints in v1a):");
                let avail: i128 = (sum_avail_bps / 10_000) as i128;
                for (mint, amt) in &liability_by_cash_mint {
                    let signed = avail - (*amt as i128);
                    let tag = if signed < 0 { "  UNDERCOLLATERALIZED" } else { "" };
                    println!(
                        "  vs {}: {avail} − {amt} = {signed}{tag}",
                        Address::new_from_array(*mint),
                    );
                }
            }
            Ok(())
        }
    }
}

fn cached_vault(
    ctx: &Ctx,
    cache: &mut std::collections::BTreeMap<Address, Vault>,
    vault_bytes: [u8; 32],
) -> Result<Vault> {
    let pda = Address::new_from_array(vault_bytes);
    if let Some(v) = cache.get(&pda) {
        return Ok(*v);
    }
    let v = read_vault_by_pda(ctx, &pda)?;
    cache.insert(pda, v);
    Ok(v)
}

/// Project a loan's simple-interest liability at `current_slot`, in
/// the loan's cash mint units. Mirrors `ssr-lending::compute_simple_interest`
/// exactly so this view matches what `repay_loan` would compute on
/// the same slot. Returns the interest amount as `u128` (loan
/// principal is also kept as u128 by the caller for summing) so the
/// liability accumulator doesn't lose precision before the final
/// per-cash-mint divide.
fn project_loan_interest(l: &Loan, current_slot: u64) -> Result<u128> {
    if l.interest_bps_per_year == 0 {
        return Ok(0);
    }
    let slots_elapsed = (current_slot as u128).saturating_sub(l.opened_slot as u128);
    if slots_elapsed == 0 {
        return Ok(0);
    }
    let numerator = (l.principal_amount as u128)
        .checked_mul(l.interest_bps_per_year as u128)
        .and_then(|x| x.checked_mul(slots_elapsed))
        .ok_or_else(|| anyhow!("interest projection overflowed for loan principal={} rate={}bps elapsed={}",
            l.principal_amount, l.interest_bps_per_year, slots_elapsed))?;
    let denom = SLOTS_PER_YEAR
        .checked_mul(BPS_DENOMINATOR)
        .ok_or_else(|| anyhow!("interest denom overflowed"))?;
    Ok(numerator / denom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssr_types::loan_status;

    fn dummy_loan(principal: u64, rate_bps: u32, opened_slot: u64) -> Loan {
        Loan {
            last_modified_slot: opened_slot,
            borrower: [1; 32],
            lender: [2; 32],
            collateral_vault: [3; 32],
            cash_vault: [4; 32],
            collateral_amount: 0,
            principal_amount: principal,
            opened_slot,
            maturity_slot: opened_slot + 1_000_000,
            nonce: 0,
            interest_bps_per_year: rate_bps,
            status: loan_status::OPEN,
            bump: 254,
            _pad: [0; 2],
            _reserved: [0; 16],
        }
    }

    #[test]
    fn project_interest_matches_program_formula_at_one_year() {
        // 1_000_000 principal @ 500 bps (5% APR) for exactly one
        // year's worth of slots = 50_000. Mirrors the canonical case
        // pinned by ssr-lending's `interest_zero_inputs_return_zero` /
        // surrounding tests and the formula in the lib.rs comment.
        let l = dummy_loan(1_000_000, 500, 100);
        let one_year_later = 100 + SLOTS_PER_YEAR as u64;
        assert_eq!(project_loan_interest(&l, one_year_later).unwrap(), 50_000);
    }

    #[test]
    fn project_interest_zero_inputs_are_zero() {
        assert_eq!(project_loan_interest(&dummy_loan(0, 500, 0), 1_000_000).unwrap(), 0);
        assert_eq!(project_loan_interest(&dummy_loan(1_000_000, 0, 0), 1_000_000).unwrap(), 0);
        assert_eq!(project_loan_interest(&dummy_loan(1_000_000, 500, 1_000_000), 1_000_000).unwrap(), 0);
        // Same-slot view (current_slot < opened_slot can happen due to
        // RPC racing the loan's open transaction): saturating sub → 0.
        assert_eq!(project_loan_interest(&dummy_loan(1_000_000, 500, 100), 50).unwrap(), 0);
    }
}

fn cmd_derive(cli: &Cli, cmd: DeriveCmd) -> Result<()> {
    // Pure derivations don't need RPC or keypair — only the program
    // IDs whose seeds we're using. Reuse ProgramIds so each command
    // requires only its own dependencies (mirror of the Ctx accessors).
    let programs = ProgramIds::from_cli(cli)?;
    match cmd {
        DeriveCmd::Record { participant } => {
            let p = Address::from_str(&participant).context("participant pubkey")?;
            let pda = Address::find_program_address(
                &[seeds::ACCOUNT_RECORD, p.as_ref()],
                &programs.compliance()?,
            )
            .0;
            println!("{pda}");
        }
        DeriveCmd::Registry => {
            let pda =
                Address::find_program_address(&[seeds::REGISTRY], &programs.compliance()?).0;
            println!("{pda}");
        }
        DeriveCmd::Vault { mint } => {
            let mint = Address::from_str(&mint).context("mint pubkey")?;
            let pda =
                Address::find_program_address(&[seeds::VAULT, mint.as_ref()], &programs.vault()?)
                    .0;
            println!("{pda}");
        }
        DeriveCmd::Position { mint, depositor } => {
            let mint = Address::from_str(&mint).context("mint pubkey")?;
            let depositor = Address::from_str(&depositor).context("depositor pubkey")?;
            let vault_program = programs.vault()?;
            let vault =
                Address::find_program_address(&[seeds::VAULT, mint.as_ref()], &vault_program).0;
            let pda = Address::find_program_address(
                &[seeds::POSITION, vault.as_ref(), depositor.as_ref()],
                &vault_program,
            )
            .0;
            println!("{pda}");
        }
        DeriveCmd::Repo {
            borrower,
            lender,
            collateral_mint,
            cash_mint,
            nonce,
        } => {
            let borrower = Address::from_str(&borrower).context("borrower")?;
            let lender = Address::from_str(&lender).context("lender")?;
            let collateral_mint = Address::from_str(&collateral_mint).context("collateral mint")?;
            let cash_mint = Address::from_str(&cash_mint).context("cash mint")?;
            let vault_program = programs.vault()?;
            let repo_program = programs.repo()?;
            let collateral_vault = Address::find_program_address(
                &[seeds::VAULT, collateral_mint.as_ref()],
                &vault_program,
            )
            .0;
            let cash_vault =
                Address::find_program_address(&[seeds::VAULT, cash_mint.as_ref()], &vault_program)
                    .0;
            let nonce_bytes = nonce.to_le_bytes();
            let pda = Address::find_program_address(
                &[
                    seeds::REPO,
                    borrower.as_ref(),
                    lender.as_ref(),
                    collateral_vault.as_ref(),
                    cash_vault.as_ref(),
                    &nonce_bytes,
                ],
                &repo_program,
            )
            .0;
            println!("{pda}");
        }
        DeriveCmd::Loan {
            borrower,
            lender,
            collateral_mint,
            cash_mint,
            nonce,
        } => {
            let borrower = Address::from_str(&borrower).context("borrower")?;
            let lender = Address::from_str(&lender).context("lender")?;
            let collateral_mint = Address::from_str(&collateral_mint).context("collateral mint")?;
            let cash_mint = Address::from_str(&cash_mint).context("cash mint")?;
            let vault_program = programs.vault()?;
            let lending_program = programs.lending()?;
            let collateral_vault = Address::find_program_address(
                &[seeds::VAULT, collateral_mint.as_ref()],
                &vault_program,
            )
            .0;
            let cash_vault =
                Address::find_program_address(&[seeds::VAULT, cash_mint.as_ref()], &vault_program)
                    .0;
            let nonce_bytes = nonce.to_le_bytes();
            let pda = Address::find_program_address(
                &[
                    seeds::LOAN,
                    borrower.as_ref(),
                    lender.as_ref(),
                    collateral_vault.as_ref(),
                    cash_vault.as_ref(),
                    &nonce_bytes,
                ],
                &lending_program,
            )
            .0;
            println!("{pda}");
        }
        DeriveCmd::Ata { owner, mint } => {
            let owner = Address::from_str(&owner).context("owner pubkey")?;
            let mint = Address::from_str(&mint).context("mint pubkey")?;
            let ata = derive_canonical_ata(
                &owner,
                &mint,
                &ssr_token_2022_id(),
                &ata_program_id(),
            );
            println!("{ata}");
        }
        DeriveCmd::SwapDvp {
            settlement_authority,
            user_a,
            user_b,
            mint_a,
            mint_b,
            nonce,
        } => {
            let sa = Address::from_str(&settlement_authority).context("settlement_authority")?;
            let ua = Address::from_str(&user_a).context("user_a")?;
            let ub = Address::from_str(&user_b).context("user_b")?;
            let ma = Address::from_str(&mint_a).context("mint_a")?;
            let mb = Address::from_str(&mint_b).context("mint_b")?;
            let nonce_bytes = nonce.to_le_bytes();
            let pda = Address::find_program_address(
                &[
                    b"dvp",
                    sa.as_ref(),
                    ua.as_ref(),
                    ub.as_ref(),
                    ma.as_ref(),
                    mb.as_ref(),
                    &nonce_bytes,
                ],
                &Address::from(SPC_DVP_PROGRAM_ID),
            )
            .0;
            println!("{pda}");
        }
    }
    Ok(())
}

// ─── Token-2022 and ATA program IDs ─────────────────────────────────────-

fn ssr_token_2022_id() -> Address {
    // TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb
    Address::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").unwrap()
}
fn ata_program_id() -> Address {
    // ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL
    Address::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap()
}
fn derive_canonical_ata(
    owner: &Address,
    mint: &Address,
    token_program: &Address,
    ata_program: &Address,
) -> Address {
    Address::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        ata_program,
    )
    .0
}

// ─── main ───────────────────────────────────────────────────────────────-

fn main() -> Result<()> {
    let mut cli = Cli::parse();
    // Pull the command out so the rest of `cli` (the global flags)
    // remains borrowable. Replaces it with a placeholder we won't touch.
    let command = std::mem::replace(
        &mut cli.command,
        TopCommand::Derive(DeriveCmd::Registry),
    );
    match command {
        TopCommand::Compliance(cmd) => {
            let ctx = Ctx::from_cli(&cli)?;
            cmd_compliance(&ctx, cmd)
        }
        TopCommand::Dvp(cmd) => {
            let ctx = Ctx::from_cli(&cli)?;
            cmd_dvp(&ctx, cmd)
        }
        TopCommand::Vault(cmd) => {
            let ctx = Ctx::from_cli(&cli)?;
            cmd_vault(&ctx, cmd)
        }
        TopCommand::Repo(cmd) => {
            let ctx = Ctx::from_cli(&cli)?;
            cmd_repo(&ctx, cmd)
        }
        TopCommand::Lending(cmd) => {
            let ctx = Ctx::from_cli(&cli)?;
            cmd_lending(&ctx, cmd)
        }
        TopCommand::Margin(cmd) => {
            let ctx = Ctx::from_cli(&cli)?;
            cmd_margin(&ctx, cmd)
        }
        TopCommand::Derive(cmd) => cmd_derive(&cli, cmd),
        TopCommand::Scenario(cmd) => cmd_scenario(cmd),
        TopCommand::ComplianceGateDemo => {
            compliance_demo::run_compliance_demo_cli();
            Ok(())
        }
    }
}

/// Dispatch for the `scenario` subcommand family. No RPC; reads from
/// `scenarios/*.json` and renders to stdout.
fn cmd_scenario(cmd: ScenarioCmd) -> Result<()> {
    match cmd {
        ScenarioCmd::List { dir } => {
            let paths = scenario::list_in(&dir)?;
            let mut loaded: Vec<(std::path::PathBuf, scenario::Scenario)> =
                Vec::with_capacity(paths.len());
            for path in paths {
                match scenario::load_from_path(&path) {
                    Ok(s) => loaded.push((path, s)),
                    Err(e) => eprintln!("warning: skipping {}: {e:#}", path.display()),
                }
            }
            print!("{}", scenario::render_list(&loaded));
            Ok(())
        }
        ScenarioCmd::Show { name, dir } => {
            let path = dir.join(format!("{name}.json"));
            let s = scenario::load_from_path(&path)?;
            print!("{}", scenario::render_show(&s, &path));
            Ok(())
        }
        ScenarioCmd::Run { name, dir, dry_run } => {
            let path = dir.join(format!("{name}.json"));
            let s = scenario::load_from_path(&path)?;
            if dry_run {
                print!("{}", scenario::render_run_v0(&s, &path));
                Ok(())
            } else {
                let report = scenario::run_embedded(&s, &path)?;
                if report.failed > 0 {
                    bail!("{} step(s) failed during scenario run", report.failed);
                }
                Ok(())
            }
        }
    }
}
