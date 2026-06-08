# ssr

**Solana Prime Broker Sandbox engine.** A runnable sandbox for exploring shared-risk brokerage on Solana — compliance-gated transfers, atomic delivery-vs-payment, cross-asset margin netting, oracle-priced liquidation — without committing to an implementation.

This is the engine behind Fabrknt's [Solana Prime Broker Sandbox](https://fabrknt.com/solana-prime-broker.html). Per `fabrknt/website/CONCEPT.md`, the sandbox exists so engineering teams, protocol designers, and risk/treasury reviewers can study how a prime-broker-shape system *behaves* on Solana under stress — what cross-margin nets, where compliance gates fire, what a 50% crash looks like through a Pyth-priced lens — by running scenarios rather than reading code.

## What you can explore

- **Atomic settlement under a compliance gate.** Hook the on-chain compliance check at the *composition* layer (DvP wrapper) rather than as a Token-2022 TransferHook. Observe how that changes throughput, error semantics, and the SPC channel compatibility envelope.
- **Cross-asset margin in USD.** A borrower's collateral in mint A backs a loan in cash mint B; the pool is haircut-adjusted, liabilities are Pyth-priced. Tighten a haircut, watch free margin recompute.
- **Negative paths.** What happens when a counterparty is suspended mid-flow? When the Pyth feed is stale? When the borrower attempts to open more debt than the pool supports? Each is a one-command exploration.
- **Compliance composition vs. transfer-hook mode.** Both are real options on this engine. The repo lets you flip between them and see the cost trade.

## Bring your own outcome

The sandbox doesn't prescribe one "right" answer. It exposes a set of mechanisms and a CLI to drive them. Buyers, partners, and adjacent teams use it to figure out which combination fits their own constraints — not to be told what theirs are.

## Engine internals (for the curious)

Underneath, `ssr` ships:

- **`crates/ssr-types`** — Pod-compatible on-chain primitives.
- **`programs/ssr-compliance`** — Pinocchio program: on-chain compliance registry + SPL Transfer-Hook-compatible gate + governance-mutable haircut table + Pyth-backed price feeds.
- **`programs/ssr-dvp-wrapper`** — Thin compliance gate over Solana Foundation's SPC `dvp-swap-program` for atomic DvP.
- **`programs/ssr-vault`** — Per-(mint, depositor) collateral position with lock-authority abstraction.
- **`programs/ssr-repo`** — Bilateral time-bound collateral lock.
- **`programs/ssr-lending`** — Term lending with on-chain margin enforcement + cross-collateral pool + Pyth-priced cross-mint liabilities.
- **`cli/`** — `ssr-cli` admin + scenario surface.
- **`tests/integration`** — LiteSVM end-to-end demos.

For deeper notes on the design choices: [`docs/composition-wrapper-pattern.md`](docs/composition-wrapper-pattern.md), [`docs/spc-integration.md`](docs/spc-integration.md), [`docs/spc-vs-ssr.md`](docs/spc-vs-ssr.md).

## What this is NOT

- Not a production deployment. Not a settlement system you would put real customer money through today. Pyth Receiver program ID is configurable; the rest of the operational posture (multisig keys, oracle redundancy, HSM custody, formal verification) is not present and isn't promised here.
- Not coupled to any specific institution. The engine is generic; the sandbox surface is meant for evaluation, not for deployment.

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

Four scenarios ship today:
- **`compliance-gate-demo`** (v2, no validator needed) — pure-Rust simulation of the compliance gate against a 5-participant population; renders a 5×5 allow/reject matrix with named failure reasons.
- `dvp-happy-path` — compliance bootstrap → atomic DvP settle (v1, sub-process).
- `dvp-suspension-reject` — suspended counterparty rejects with distinct `COMPLIANCE_SUSPENDED` (v1, sub-process).
- `cross-margin-view` — cross-asset margin pool + cross-mint loan, Pyth-driven (v1, sub-process).

### Drive the CLI directly

```bash
# Boot a local Solana validator (separate terminal).
solana-test-validator --reset

# Build + deploy the programs.
cargo build-sbf --manifest-path programs/ssr-compliance/Cargo.toml
cargo build-sbf --manifest-path programs/ssr-dvp-wrapper/Cargo.toml

solana program deploy target/deploy/ssr_compliance.so
solana program deploy target/deploy/ssr_dvp_wrapper.so

# Drive the CLI command-by-command — same flows the scenarios use,
# without the scenario wrapper.
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

The CLI command catalogue in [`cli/README.md`](cli/README.md) covers every flag.

## Sandbox elements: current state

Per `fabrknt/website/SANDBOX-PATTERN.md`, every Fabrknt sandbox must ship five elements. Honest status:

| Element | Status | Notes |
|---|---|---|
| (1) Pre-baked scenarios | **present** | 3 scenarios in `scenarios/`. More to follow (repo lifecycle, lending happy-path, oracle-priced liquidation). |
| (2) Business-readable output | **v2 done for v2-eligible scenarios** | Scenarios whose steps are all in-process-eligible (currently: `compliance-gate-demo`) take the v2 path: in-process dispatch into `run_compliance_demo_structured`, then HEADLINE (with ✓/⚠/unverified badge per `expected_outcomes` verification), TIMELINE, DELTA, OUTCOMES, NEXT. The shipped v2 scenario declares 6 outcomes that all verify ✓. The DVP / cross-margin scenarios still spawn `ssr-cli` sub-processes with stdio inherited (v1) because their steps depend on a running validator + deployed programs; adding more in-process demos (cross-margin sim, repo-lifecycle sim) is tracked in `fabrknt/website/SANDBOX-BACKLOG.md`. |
| (3) Parameter dial | partial | Risk params (haircut table, max staleness) are governance-mutable. Per-step `--mint`, `--participant`, etc. provide dial-style overrides in scenarios. CLI-flag override on `scenario run` (e.g. `--haircut equity 4500`) is v2. |
| (4) Scenario replay | **present** | Each scenario file is a deterministic step list — re-running yields the same sub-process invocations. Bit-identical chain state requires `solana-test-validator --reset` between runs. |
| (5) CTA | **done** | `scenario list` / `show` / `run` all render a three-option CTA footer (adopt engine / custom build / hosted access) with `product=solana-prime-broker` for waitlist enrichment. |

## Related

- [`fabrknt/website/CONCEPT.md`](../fabrknt/website/CONCEPT.md) — Fabrknt brand and 2×2 sandbox structure.
- [`fabrknt/website/SANDBOX-PATTERN.md`](../fabrknt/website/SANDBOX-PATTERN.md) — cross-engine 5-element spec.
- Sibling Fabrknt engines: [`rdk/openhl`](../rdk/openhl/), [`rdk/princeps`](../rdk/princeps/), [`openhl-solana`](../openhl-solana/).
- [`docs/composition-wrapper-pattern.md`](docs/composition-wrapper-pattern.md) — composition-layer compliance design.
- [`docs/spc-integration.md`](docs/spc-integration.md) — Solana Foundation SPC integration analysis.
- [`docs/spc-vs-ssr.md`](docs/spc-vs-ssr.md) — 1-page position memo on SSR vs SPC.

## License

Apache-2.0 OR MIT.
