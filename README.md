# ssr

**Solana Prime Broker Sandbox engine.** Native Solana programs implementing institutional RWA prime broker primitives — compliance-gated mint, DvP atomic settlement, collateral vault, repo, term lending, oracle-priced cross-margin — designed to make shared-risk brokerage on Solana explorable.

This is the engine behind Fabrknt's [Solana Prime Broker Sandbox](https://fabrknt.com/solana-prime-broker.html). Per `fabrknt/website/CONCEPT.md`, the sandbox exists so engineering teams, treasury/risk officers, and product owners can study how a Solana-native prime broker system behaves — what changes vs an EVM prime broker, what does it look like to compose compliance + DvP + cross-margin on Solana — by running scenarios rather than reading code.

## What it is

`ssr` ships:

- **`crates/ssr-types`** — Pod-compatible on-chain primitives (`AccountRecord`, `Registry`, status / jurisdiction / asset-class / role discriminants, PDA seeds, status-transition policy).
- **`programs/ssr-compliance`** — Pinocchio program: on-chain KYC/AML/accredited registry + SPL Transfer Hook–compatible gate + governance-mutable haircut table + Pyth-backed price feeds.
- **`programs/ssr-dvp-wrapper`** — Thin compliance gate over Solana Foundation's SPC `dvp-swap-program` for atomic Delivery-vs-Payment.
- **`programs/ssr-vault`** — Per-(mint, depositor) collateral position with lock-authority abstraction (Phase 2).
- **`programs/ssr-repo`** — Bilateral time-bound collateral lock for repo (Phase 3).
- **`programs/ssr-lending`** — Term lending with on-chain margin enforcement + cross-collateral pool + Pyth-priced cross-mint liabilities (Phase 3–4).
- **`cli/`** — Admin and demo surface (`ssr-cli`); every command maps 1-to-1 to an on-chain instruction or a pure derivation.
- **`tests/integration`** — LiteSVM end-to-end demos exercising the full Phase 0→4 composition.

Architecture choices that make this an interesting sandbox surface:

- **Two compliance modes coexist** — Token-2022 TransferHook (continuous on-chain gating) AND the composition wrapper pattern (gate-per-meaningful-action). See [`docs/composition-wrapper-pattern.md`](docs/composition-wrapper-pattern.md).
- **Atomic DvP via Foundation primitives** — uses SPC's audited-ish `dvp-swap-program` rather than reinventing the wheel. See [`docs/spc-integration.md`](docs/spc-integration.md) and [`docs/spc-vs-ssr.md`](docs/spc-vs-ssr.md).
- **Cross-margin in USD** — Pyth-priced collateral pool vs liabilities, with governance-mutable haircuts per asset class (Phase 4).
- **Conservative pricing** — Pyth `price − conf`, mandatory exponent normalization, owner-program validation behind opt-in PDA.

## What it is NOT

- Not production. No mainnet deployment. The Pyth Receiver program ID is configurable but the rest of the operational posture (multisig keys, oracle redundancy, HSM custody, formal verification) is not present.
- Not a full RWA platform — token issuance, settlement-network connectivity, KYC vendor integration, fiat rails are out of scope.
- Not coupled to any specific institution. The engine is generic; institutional integrations live in private engagement workspaces, not here.

## How to explore (today)

### Sandbox surface (recommended starting point)

```bash
# Discover the curated scenarios.
ssr-cli scenario list

# Inspect one without running.
ssr-cli scenario show dvp-happy-path

# Run the scenario: each step is spawned as a sub-process (ssr-cli
# prefixes auto-route to the current binary; other CLIs run from
# PATH). Stdio inherited so step output streams live.
ssr-cli scenario run dvp-happy-path

# Pass --dry-run to print the step list without executing.
ssr-cli scenario run dvp-happy-path --dry-run
```

Three scenarios ship today: `dvp-happy-path` (compliance bootstrap → atomic DvP settle), `dvp-suspension-reject` (negative path with distinct `COMPLIANCE_SUSPENDED` error), `cross-margin-view` (Phase 4 unified margin across two collateral mints + one cross-mint loan).

### Drive the CLI directly

```bash
# Boot a local Solana validator (separate terminal).
solana-test-validator --reset

# Build + deploy the programs.
cargo build-sbf --manifest-path programs/ssr-compliance/Cargo.toml
cargo build-sbf --manifest-path programs/ssr-dvp-wrapper/Cargo.toml
# (and ssr-vault / ssr-repo / ssr-lending similarly)

solana program deploy target/deploy/ssr_compliance.so
solana program deploy target/deploy/ssr_dvp_wrapper.so

# Drive the CLI end-to-end (see cli/README.md "Demo dramaturgy" steps 1–13).
ssr-cli compliance init-registry
ssr-cli compliance register --participant <pk> --jurisdiction JP
ssr-cli compliance verify   --participant <pk>
ssr-cli dvp settle          --swap-dvp <pk>
ssr-cli vault deposit       --mint <pk> --amount 500000
ssr-cli repo open           --borrower-keypair … --lender-keypair … …
ssr-cli lending open        --borrower-keypair … --lender-keypair … …
ssr-cli margin show         --user <pk> --mint <pk> [--mint <pk> …]

# End-to-end LiteSVM walk (in-process; no validator needed):
cargo test -p ssr-integration-tests --test demo_walk
```

The CLI demo walk in [`cli/README.md`](cli/README.md) is the closest thing to a guided sandbox tour today.

## Sandbox elements: current state

Per `fabrknt/website/SANDBOX-PATTERN.md`, every Fabrknt sandbox must ship five elements. Here is `ssr`'s current state:

| Element | Status | Notes |
|---|---|---|
| (1) Pre-baked scenarios | **present** | `scenarios/` directory with 3 scenarios (`dvp-happy-path`, `dvp-suspension-reject`, `cross-margin-view`). More to follow (repo lifecycle, lending happy-path, oracle-priced liquidation). |
| (2) Business-readable output | **v1 present** | `ssr-cli scenario run` spawns each step as a sub-process (`ssr-cli` prefixes auto-route to `current_exe`; other CLIs from PATH), wrapped with a headline header, per-step separators, and a final pass/fail verdict. Stdio inherited so step output streams live. v2 will tee stdio so declared expect-substrings can be verified. |
| (3) Parameter dial | partial | Risk params (haircut table, max staleness) are governance-mutable. Per-step `--mint`, `--participant`, etc. provide dial-style overrides in scenarios. |
| (4) Scenario replay | **present** | Each scenario file is a deterministic step list — re-running yields the same sub-process invocations. Bit-identical state requires `solana-test-validator --reset` between runs. |
| (5) CTA | **done** | `ssr-cli scenario list` / `show` / `run` all render a three-option CTA footer (adopt engine / custom build / hosted access) with `product=solana-prime-broker` for waitlist enrichment. |

## Build

```bash
cargo check --workspace            # workspace check (host-side)
cargo test --workspace             # unit + integration tests
cargo build-sbf --manifest-path programs/ssr-compliance/Cargo.toml
# (repeat for each program crate)
```

## Related

- [`fabrknt/website/CONCEPT.md`](../fabrknt/website/CONCEPT.md) — Fabrknt brand and 2x2 sandbox structure.
- [`fabrknt/website/SANDBOX-PATTERN.md`](../fabrknt/website/SANDBOX-PATTERN.md) — cross-engine spec for the five sandbox elements.
- Sibling Fabrknt engines: [`rdk/openhl`](../rdk/openhl/) (EVM Perp), [`rdk/princeps`](../rdk/princeps/) (EVM Prime Broker), [`openhl-solana`](../openhl-solana/) (Solana Perp).
- [`docs/composition-wrapper-pattern.md`](docs/composition-wrapper-pattern.md) — the Model C compliance pattern.
- [`docs/spc-integration.md`](docs/spc-integration.md) — Solana Foundation SPC integration analysis.
- [`docs/spc-vs-ssr.md`](docs/spc-vs-ssr.md) — 1-page position memo on SSR vs SPC.

## License

Apache-2.0 OR MIT.
