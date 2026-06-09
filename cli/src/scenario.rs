//! **Mirrored across sibling Fabrknt sandbox engines.** This module's
//! shape (Scenario JSON, list/show/run renderers, run_embedded with
//! sub-process spawn + stdio inherit, `has_shell_metacharacters`,
//! `EmbeddedReport`, CTA footer with `product=` waitlist enrichment)
//! is duplicated nearly verbatim in:
//!
//!   - psyto/rdk            → `princeps/bin/princeps/src/scenario.rs`
//!   - psyto/openhl-solana  → `scripts/scenario/src/main.rs`
//!
//! When you change behavior shared across them (metachar detection
//! rules, headline rendering, JSON shape, CTA footer text), apply the
//! same change to all three. The decision to keep three copies rather
//! than extract to a shared crate (e.g. `fabrknt-scenario-runner`) is
//! deliberate: the engines live in different repos with no shared
//! workspace, so a crate would need crates.io publication + cross-repo
//! version coordination that doesn't yet pay for itself given the
//! small surface area. Revisit when adding the 4th subprocess-based
//! runner or when the shared surface grows.
//!
//! The 4th Fabrknt runner (`rdk/openhl`) is structurally different —
//! in-process execution via `LiveRethEvmBridge<()>` rather than
//! sub-process spawn — so it shares only the Scenario JSON shape and
//! the CTA footer with these three.
//!
//! ---
//!
//! Sandbox scenario surface for ssr-cli.
//!
//! A *scenario* is a metadata-wrapped recipe of `ssr-cli` invocations
//! that demonstrate one institutional Solana prime broker behavior
//! end-to-end. Per `fabrknt/website/SANDBOX-PATTERN.md`, this surface
//! implements: (1) pre-baked scenarios in `scenarios/*.json`, (2) ASCII
//! headline + step rendering, (3) parameter dial via per-step args,
//! (4) replay (the JSON IS the replay format), (5) CTA footer.
//!
//! v1 spawns each step as a sub-process via `std::env::current_exe`
//! so `ssr-cli`-prefixed commands re-enter the same binary; other
//! commands (e.g., `spl-token`, `solana-keygen`, `solana program
//! deploy`) are spawned by name and require the relevant CLI to be in
//! PATH. Stdio is inherited so each step's output streams live.
//!
//! v0 (legacy [`render_run_v0`]) prints only the step list; kept
//! exported for the `--dry-run` flag.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub name: String,
    pub category: String,
    pub description: String,
    pub headline: String,
    pub steps: Vec<ScenarioStep>,
    /// v2 Phase 3: optional declarative outcome checks. When present
    /// and the runner is v2-eligible, each check is evaluated against
    /// the captured execution result and rendered as `✓` / `✗` in the
    /// OUTCOMES section. When all checks pass, the HEADLINE drops the
    /// "(unverified)" qualifier.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_outcomes: Vec<ExpectedOutcome>,
}

/// One declarative outcome a scenario claims to demonstrate. `check`
/// is engine-specific; for ssr it is a [`ComplianceCheck`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedOutcome {
    pub name: String,
    pub description: String,
    pub check: ComplianceCheck,
}

/// Engine-specific check schema for ssr. JSON-serialized as externally-
/// tagged so authors write `{"allowed_pairs": 2}` rather than
/// `{"kind": "allowed_pairs", "value": 2}`. Unit variants serialize as
/// bare strings.
///
/// Checks dispatch on their own variant to the matching `StepResult`
/// type: compliance-gate checks look at the last `ComplianceGateDemo`
/// result; haircut checks at the last `HaircutMatrix` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComplianceCheck {
    // --- ComplianceGateDemo checks ---
    /// Assert the exact count of (sender, receiver) pairs the gate allows.
    AllowedPairs(usize),
    /// Assert the exact count of pairs the gate rejects.
    RejectedPairs(usize),
    /// Assert that a specific named pair clears the gate.
    AllowedPair {
        sender: String,
        receiver: String,
    },
    /// Assert that a specific named pair is rejected by the gate.
    RejectedPair {
        sender: String,
        receiver: String,
    },
    /// Assert that a specific named pair is rejected with a reason
    /// that contains the given substring (case-sensitive).
    RejectReason {
        sender: String,
        receiver: String,
        reason_contains: String,
    },

    // --- HaircutMatrix checks ---
    /// Assert exact count of recognised asset classes (8 by default).
    HaircutClassCountExact(usize),
    /// Assert a specific asset class's default haircut equals the value.
    HaircutForClassExact { class: String, expected_bps: u16 },
    /// Assert the matrix is ordered safest-first (monotonic non-decreasing).
    HaircutMonotonicByRiskTier,
    /// Assert at least N asset classes carry zero haircut (the "near-cash" tier).
    HaircutZeroClassCountMin(usize),
    /// Assert the heaviest haircut across all classes is at least N bps.
    HaircutHeaviestAtLeast(u16),
}

/// Per-outcome evaluation status used in the OUTCOMES section.
#[derive(Debug, Clone)]
pub enum OutcomeStatus {
    Pass,
    Fail(String),
}

/// Evaluate one ComplianceGate-targeted check.
pub fn evaluate_compliance_check(
    check: &ComplianceCheck,
    result: &crate::compliance_demo::ComplianceDemoResult,
) -> OutcomeStatus {
    match check {
        ComplianceCheck::AllowedPairs(expected) => {
            let observed = result.allowed_pairs();
            if observed == *expected {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!("observed allowed_pairs = {observed}"))
            }
        }
        ComplianceCheck::RejectedPairs(expected) => {
            let observed = result.rejected_pairs();
            if observed == *expected {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!("observed rejected_pairs = {observed}"))
            }
        }
        ComplianceCheck::AllowedPair { sender, receiver } => {
            match find_outcome(result, sender, receiver) {
                Some(o) if o.allowed => OutcomeStatus::Pass,
                Some(o) => OutcomeStatus::Fail(format!("pair {sender}→{receiver} rejected: {}", o.reason)),
                None => OutcomeStatus::Fail(format!("pair {sender}→{receiver} not found (check names)")),
            }
        }
        ComplianceCheck::RejectedPair { sender, receiver } => {
            match find_outcome(result, sender, receiver) {
                Some(o) if !o.allowed => OutcomeStatus::Pass,
                Some(_) => OutcomeStatus::Fail(format!("pair {sender}→{receiver} was allowed (expected reject)")),
                None => OutcomeStatus::Fail(format!("pair {sender}→{receiver} not found")),
            }
        }
        ComplianceCheck::RejectReason {
            sender,
            receiver,
            reason_contains,
        } => match find_outcome(result, sender, receiver) {
            Some(o) if o.allowed => OutcomeStatus::Fail(format!(
                "pair {sender}→{receiver} was allowed (expected reject containing '{reason_contains}')"
            )),
            Some(o) if o.reason.contains(reason_contains) => OutcomeStatus::Pass,
            Some(o) => OutcomeStatus::Fail(format!(
                "pair {sender}→{receiver} rejected but reason '{}' did not contain '{reason_contains}'",
                o.reason
            )),
            None => OutcomeStatus::Fail(format!("pair {sender}→{receiver} not found")),
        },
        // Haircut checks are evaluated by `evaluate_haircut_check`.
        ComplianceCheck::HaircutClassCountExact(_)
        | ComplianceCheck::HaircutForClassExact { .. }
        | ComplianceCheck::HaircutMonotonicByRiskTier
        | ComplianceCheck::HaircutZeroClassCountMin(_)
        | ComplianceCheck::HaircutHeaviestAtLeast(_) => OutcomeStatus::Fail(
            "this check targets the haircut matrix, not the compliance gate".to_string(),
        ),
    }
}

/// Evaluate one HaircutMatrix-targeted check.
pub fn evaluate_haircut_check(
    check: &ComplianceCheck,
    result: &crate::haircut_matrix_demo::HaircutMatrixDemoResult,
) -> OutcomeStatus {
    match check {
        ComplianceCheck::HaircutClassCountExact(expected) => {
            if result.class_count() == *expected {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed class count = {}",
                    result.class_count()
                ))
            }
        }
        ComplianceCheck::HaircutForClassExact { class, expected_bps } => {
            match result.find_class(class) {
                Some(e) if e.haircut_bps == *expected_bps => OutcomeStatus::Pass,
                Some(e) => OutcomeStatus::Fail(format!(
                    "class {class} haircut = {} bps (expected {expected_bps})",
                    e.haircut_bps
                )),
                None => OutcomeStatus::Fail(format!("class {class} not found")),
            }
        }
        ComplianceCheck::HaircutMonotonicByRiskTier => {
            if result.haircuts_monotonic_non_decreasing() {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail("matrix ordering breaks the risk-tier monotonicity".to_string())
            }
        }
        ComplianceCheck::HaircutZeroClassCountMin(min) => {
            let zero_count = result.entries.iter().filter(|e| e.haircut_bps == 0).count();
            if zero_count >= *min {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed zero-haircut classes = {zero_count}"
                ))
            }
        }
        ComplianceCheck::HaircutHeaviestAtLeast(min) => {
            let max = result.entries.iter().map(|e| e.haircut_bps).max().unwrap_or(0);
            if max >= *min {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!("observed heaviest haircut = {max} bps"))
            }
        }
        // Non-haircut checks don't apply here.
        _ => OutcomeStatus::Fail(
            "this check targets the compliance gate, not the haircut matrix".to_string(),
        ),
    }
}

fn find_outcome<'a>(
    result: &'a crate::compliance_demo::ComplianceDemoResult,
    sender: &str,
    receiver: &str,
) -> Option<&'a crate::compliance_demo::TransferOutcome> {
    let s_idx = result.participants.iter().position(|p| p.name == sender)?;
    let r_idx = result.participants.iter().position(|p| p.name == receiver)?;
    result
        .matrix
        .iter()
        .find(|t| t.sender_idx == s_idx && t.receiver_idx == r_idx)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioStep {
    pub explanation: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<String>,
}

pub fn load_from_path(path: &Path) -> Result<Scenario> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let s: Scenario =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    Ok(s)
}

pub fn list_in(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Err(anyhow!(
            "scenarios directory not found: {}\n\
            run `ssr-cli scenario list` from the ssr repo root, or pass --dir explicitly.",
            dir.display()
        ));
    }
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    Ok(paths)
}

pub fn render_list(scenarios: &[(PathBuf, Scenario)]) -> String {
    if scenarios.is_empty() {
        return "No scenarios found.\n".to_string();
    }
    let name_w = scenarios.iter().map(|(_, s)| s.name.len()).max().unwrap_or(8).max(4);
    let cat_w = scenarios
        .iter()
        .map(|(_, s)| s.category.len())
        .max()
        .unwrap_or(8)
        .max(8);

    let mut out = String::new();
    out.push_str(&format!(
        "{:<name_w$}  {:<cat_w$}  Headline\n",
        "Name",
        "Category",
        name_w = name_w,
        cat_w = cat_w,
    ));
    out.push_str(&format!(
        "{:-<name_w$}  {:-<cat_w$}  {:-<60}\n",
        "",
        "",
        "",
        name_w = name_w,
        cat_w = cat_w,
    ));
    for (_, s) in scenarios {
        out.push_str(&format!(
            "{:<name_w$}  {:<cat_w$}  {}\n",
            s.name,
            s.category,
            s.headline,
            name_w = name_w,
            cat_w = cat_w,
        ));
    }
    out.push_str(&cta_footer());
    out
}

pub fn render_show(scenario: &Scenario, path: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!("─── {} ────────────────────────────────────\n", scenario.headline));
    out.push_str(&format!("name        : {}\n", scenario.name));
    out.push_str(&format!("category    : {}\n", scenario.category));
    out.push_str(&format!("source      : {}\n\n", path.display()));
    out.push_str("description :\n");
    for line in scenario.description.lines() {
        out.push_str(&format!("  {line}\n"));
    }
    out.push_str(&format!("\nsteps       : {} command(s)\n", scenario.steps.len()));
    for (i, step) in scenario.steps.iter().enumerate() {
        out.push_str(&format!("  [{}] {}\n", i + 1, step.explanation));
    }
    out.push_str(&cta_footer());
    out
}

pub fn render_run_v0(scenario: &Scenario, path: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "─── scenario: {} ────────────────────────────────────\n",
        scenario.name
    ));
    out.push_str(&format!("HEADLINE: {}\n\n", scenario.headline));
    out.push_str("DESCRIPTION:\n");
    for line in scenario.description.lines() {
        out.push_str(&format!("  {line}\n"));
    }

    out.push_str("\nPREREQUISITES:\n");
    out.push_str("  # local validator running:\n");
    out.push_str("  solana-test-validator --reset\n");
    out.push_str("  # build + deploy the on-chain programs:\n");
    out.push_str("  cargo build-sbf --manifest-path programs/ssr-compliance/Cargo.toml\n");
    out.push_str("  cargo build-sbf --manifest-path programs/ssr-dvp-wrapper/Cargo.toml\n");
    out.push_str("  solana program deploy target/deploy/ssr_compliance.so\n");
    out.push_str("  solana program deploy target/deploy/ssr_dvp_wrapper.so\n");

    out.push_str(&format!("\nSTEPS ({} command(s)):\n", scenario.steps.len()));
    for (i, step) in scenario.steps.iter().enumerate() {
        out.push_str(&format!("\n  Step {} — {}\n", i + 1, step.explanation));
        out.push_str(&format!("    $ {}\n", step.command));
        if let Some(expect) = &step.expect {
            out.push_str(&format!("    # expect output to include: {expect}\n"));
        }
    }

    out.push_str(&format!("\nSOURCE: {}\n", path.display()));
    out.push_str("\nNOTE: v0 prints the step list rather than executing each command\n");
    out.push_str("in-process. Embedded execution with state-diff rendering lands in v1.\n");

    out.push_str(&cta_footer());
    out
}

/// Detect whether `command` contains shell metacharacters that mean it
/// can't be naïvely whitespace-split into argv. Returns true for
/// command chains (`&&`, `||`, `;`) and pipes (`|`).
///
/// **Intentionally excluded**: `<` and `>`. Curated scenarios use
/// `<PLACEHOLDER>` syntax for values the operator must substitute,
/// and treating `<X>` as shell redirects breaks every placeholder
/// step. Real shell redirects are not used in any shipped scenario;
/// if a future scenario genuinely needs one, prefer splitting into
/// separate steps or extending this detection more carefully.
pub fn has_shell_metacharacters(command: &str) -> bool {
    command.contains("&&")
        || command.contains("||")
        || command.contains(';')
        || command.contains('|')
}

fn cta_footer() -> String {
    let mut out = String::new();
    out.push_str("\nNEXT:\n");
    out.push_str("  • Adopt this engine  : https://github.com/psyto/ssr\n");
    out.push_str("  • Custom build       : https://fabrknt.com/waitlist.html?product=solana-prime-broker&intent=build\n");
    out.push_str("  • Hosted access      : https://fabrknt.com/waitlist.html?product=solana-prime-broker&intent=hosted\n");
    out
}

#[derive(Debug, Clone, Copy)]
pub struct EmbeddedReport {
    pub total: usize,
    pub skipped: usize,
    pub passed: usize,
    pub failed: usize,
    pub expectations_unverified: usize,
}

/// Dispatch target for a step whose command can be served in-process
/// (no sub-process spawn). v2 walks every step trying to parse one of
/// these; if every step matches, the runner takes the v2 path and
/// renders the structured HEADLINE / TIMELINE / DELTA / OUTCOMES /
/// NEXT contract from `SANDBOX-PATTERN.md`. Otherwise the runner
/// falls back to v1 (sub-process spawn with stdio inherit).
#[derive(Debug, Clone, Copy)]
pub enum InProcessTarget {
    /// `ssr-cli compliance-gate-demo`
    ComplianceGateDemo,
    /// `ssr-cli haircut-matrix-demo`
    HaircutMatrixDemo,
}

/// Try to parse a step's command into an in-process target. Returns
/// `None` for commands that must still spawn a sub-process.
pub fn try_parse_in_process(command: &str) -> Option<InProcessTarget> {
    let trimmed = command.trim();
    if trimmed == "ssr-cli compliance-gate-demo" {
        return Some(InProcessTarget::ComplianceGateDemo);
    }
    if trimmed == "ssr-cli haircut-matrix-demo" {
        return Some(InProcessTarget::HaircutMatrixDemo);
    }
    None
}

/// True iff every step in `scenario` can be served in-process.
pub fn is_v2_eligible(scenario: &Scenario) -> bool {
    !scenario.steps.is_empty()
        && scenario
            .steps
            .iter()
            .all(|s| try_parse_in_process(&s.command).is_some())
}

/// Result of running a single in-process step.
#[derive(Debug, Clone)]
enum StepResult {
    ComplianceGateDemo(crate::compliance_demo::ComplianceDemoResult),
    HaircutMatrix(crate::haircut_matrix_demo::HaircutMatrixDemoResult),
}

/// Embedded execution. For v2-eligible scenarios, dispatches in-process,
/// captures structured results, and emits the 5-section v2 output
/// contract. Otherwise falls back to v1: spawn each step's command as
/// a sub-process with stdio inherited.
pub fn run_embedded(scenario: &Scenario, path: &Path) -> Result<EmbeddedReport> {
    if is_v2_eligible(scenario) {
        return run_embedded_v2(scenario, path);
    }
    run_embedded_v1(scenario, path)
}

fn run_embedded_v2(scenario: &Scenario, path: &Path) -> Result<EmbeddedReport> {
    println!(
        "─── scenario: {} ────────────────────────────────────",
        scenario.name
    );
    println!();

    let mut results: Vec<StepResult> = Vec::with_capacity(scenario.steps.len());
    let mut failed = 0usize;

    for step in &scenario.steps {
        let target = try_parse_in_process(&step.command)
            .expect("v2-eligible scenario must have all-in-process steps");
        match target {
            InProcessTarget::ComplianceGateDemo => {
                let r = crate::compliance_demo::run_compliance_demo_structured();
                results.push(StepResult::ComplianceGateDemo(r));
            }
            InProcessTarget::HaircutMatrixDemo => {
                let r = crate::haircut_matrix_demo::run_haircut_matrix_demo_structured();
                results.push(StepResult::HaircutMatrix(r));
            }
        }
        let _ = step;
        let _ = &mut failed;
    }

    let report = EmbeddedReport {
        total: scenario.steps.len(),
        skipped: 0,
        passed: results.len(),
        failed,
        expectations_unverified: 0,
    };

    render_v2_sections(scenario, path, &results, &report);

    Ok(report)
}

fn render_v2_sections(
    scenario: &Scenario,
    path: &Path,
    results: &[StepResult],
    report: &EmbeddedReport,
) {
    // Evaluate expected_outcomes up-front so HEADLINE can carry a
    // verification badge.
    let evaluated_outcomes = evaluate_all_outcomes(scenario, results);
    let any_failed = evaluated_outcomes
        .iter()
        .any(|(_, status)| matches!(status, OutcomeStatus::Fail(_)));
    let has_outcomes = !evaluated_outcomes.is_empty();

    if has_outcomes && !any_failed {
        println!("HEADLINE ✓: {}", scenario.headline);
    } else if has_outcomes && any_failed {
        println!("HEADLINE ⚠: {}", scenario.headline);
    } else {
        println!("HEADLINE (unverified): {}", scenario.headline);
    }
    println!();

    // TIMELINE
    println!("TIMELINE:");
    for r in results {
        match r {
            StepResult::ComplianceGateDemo(d) => {
                println!(
                    "    seed       {} synthetic participants (1 each of the four statuses, plus a second VERIFIED counterparty)",
                    d.participants.len()
                );
                println!(
                    "    evaluate   every (sender, receiver) pair through `AccountRecord::check_transfer_allowed` ({} pairs)",
                    d.total_pairs()
                );
                println!(
                    "    verdict    {} allowed, {} rejected (with named reason per reject)",
                    d.allowed_pairs(),
                    d.rejected_pairs()
                );
            }
            StepResult::HaircutMatrix(d) => {
                println!(
                    "    enumerate  {} recognised SSR asset classes via ssr-types::default_haircut_bps",
                    d.class_count()
                );
                println!(
                    "    project    haircut-adjusted collateral at notional sizes {:?}",
                    d.sample_notionals
                );
                let max = d.entries.iter().map(|e| e.haircut_bps).max().unwrap_or(0);
                let zeros = d.entries.iter().filter(|e| e.haircut_bps == 0).count();
                println!(
                    "    summarize  {zeros} class(es) at zero haircut; heaviest haircut = {max} bps ({:.1}%)",
                    max as f64 / 100.0
                );
            }
        }
    }
    println!();

    // DELTA — the matrix(es).
    println!("DELTA:");
    for r in results {
        match r {
            StepResult::ComplianceGateDemo(d) => {
                print!("  sender \\ receiver   ");
                for p in &d.participants {
                    print!("  {:<9}", p.name);
                }
                println!();
                print!("  ───────────────     ");
                for _ in &d.participants {
                    print!("  ─────────");
                }
                println!();
                for (i, sender) in d.participants.iter().enumerate() {
                    print!(
                        "  {:<8}({:<9})  ",
                        sender.name,
                        truncate(sender.status_label, 9)
                    );
                    for j in 0..d.participants.len() {
                        if i == j {
                            print!("  {:<9}", "—");
                            continue;
                        }
                        let outcome = d
                            .matrix
                            .iter()
                            .find(|t| t.sender_idx == i && t.receiver_idx == j)
                            .expect("matrix is exhaustive");
                        print!(
                            "  {:<9}",
                            if outcome.allowed { "allow" } else { "reject" }
                        );
                    }
                    println!();
                }
                println!();
                println!("  Rejected pairs (sender / receiver / reason):");
                for outcome in d.matrix.iter().filter(|t| !t.allowed) {
                    let sender = &d.participants[outcome.sender_idx];
                    let receiver = &d.participants[outcome.receiver_idx];
                    println!(
                        "    {:<7} → {:<7}   {}",
                        sender.name, receiver.name, outcome.reason
                    );
                }
            }
            StepResult::HaircutMatrix(d) => {
                println!(
                    "  {:<18}  {:<8}  {:>12}  {:>12}  {:>14}",
                    "Class", "Haircut", "10K notional", "100K notional", "1M notional"
                );
                println!(
                    "  {}  {}  {}  {}  {}",
                    "─".repeat(18),
                    "─".repeat(8),
                    "─".repeat(12),
                    "─".repeat(12),
                    "─".repeat(14)
                );
                for (i, e) in d.entries.iter().enumerate() {
                    let pct = e.haircut_bps as f64 / 100.0;
                    print!("  {:<18}  {:>5.1}%   ", e.label, pct);
                    for &notional in &d.sample_notionals {
                        print!(" {:>12}", d.effective_value(i, notional));
                    }
                    println!();
                }
            }
        }
    }
    println!();

    // OUTCOMES — render each evaluated outcome.
    println!("OUTCOMES:");
    if evaluated_outcomes.is_empty() {
        println!("  (no expected_outcomes declared — HEADLINE shown as unverified)");
    } else {
        for (outcome, status) in &evaluated_outcomes {
            match status {
                OutcomeStatus::Pass => println!("  ✓ {}", outcome.description),
                OutcomeStatus::Fail(why) => {
                    println!("  ✗ {} ({why})", outcome.description);
                }
            }
        }
        let passed = evaluated_outcomes
            .iter()
            .filter(|(_, s)| matches!(s, OutcomeStatus::Pass))
            .count();
        let total = evaluated_outcomes.len();
        println!();
        println!("  {passed} of {total} outcome(s) verified.");
    }
    println!();

    if report.failed > 0 {
        println!(
            "({} of {} step(s) failed during execution)",
            report.failed, report.total
        );
        println!();
    }
    println!("source: {}", path.display());

    print!("{}", cta_footer());
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}

fn evaluate_all_outcomes<'a>(
    scenario: &'a Scenario,
    results: &[StepResult],
) -> Vec<(&'a ExpectedOutcome, OutcomeStatus)> {
    let last_compliance = results.iter().rev().find_map(|r| match r {
        StepResult::ComplianceGateDemo(d) => Some(d),
        _ => None,
    });
    let last_haircut = results.iter().rev().find_map(|r| match r {
        StepResult::HaircutMatrix(d) => Some(d),
        _ => None,
    });

    scenario
        .expected_outcomes
        .iter()
        .map(|outcome| {
            let status = match &outcome.check {
                // Compliance-gate-targeted checks
                ComplianceCheck::AllowedPairs(_)
                | ComplianceCheck::RejectedPairs(_)
                | ComplianceCheck::AllowedPair { .. }
                | ComplianceCheck::RejectedPair { .. }
                | ComplianceCheck::RejectReason { .. } => match last_compliance {
                    Some(d) => evaluate_compliance_check(&outcome.check, d),
                    None => OutcomeStatus::Fail(
                        "no compliance-demo result available".to_string(),
                    ),
                },
                // Haircut-matrix-targeted checks
                ComplianceCheck::HaircutClassCountExact(_)
                | ComplianceCheck::HaircutForClassExact { .. }
                | ComplianceCheck::HaircutMonotonicByRiskTier
                | ComplianceCheck::HaircutZeroClassCountMin(_)
                | ComplianceCheck::HaircutHeaviestAtLeast(_) => match last_haircut {
                    Some(d) => evaluate_haircut_check(&outcome.check, d),
                    None => OutcomeStatus::Fail(
                        "no haircut-matrix result available".to_string(),
                    ),
                },
            };
            (outcome, status)
        })
        .collect()
}

/// v1 embedded execution. Walks each step and spawns:
/// - `ssr-cli`-prefixed commands → current_exe (so PATH doesn't need
///   `ssr-cli` installed)
/// - other commands (spl-token, solana-keygen, solana program deploy
///   etc.) → spawned by name; missing binaries surface as a clear
///   "failed to spawn" error and mark the step failed.
///
/// Comment-only steps (`#` prefix) are skipped as informational.
fn run_embedded_v1(scenario: &Scenario, path: &Path) -> Result<EmbeddedReport> {
    println!(
        "─── scenario: {} ────────────────────────────────────",
        scenario.name
    );
    println!("HEADLINE (curator claim): {}", scenario.headline);
    println!();
    println!("DESCRIPTION:");
    for line in scenario.description.lines() {
        println!("  {line}");
    }
    println!();
    println!("PREREQUISITES (operator's responsibility):");
    println!("  solana-test-validator --reset    # in another terminal");
    println!("  cargo build-sbf --manifest-path programs/ssr-compliance/Cargo.toml");
    println!("  cargo build-sbf --manifest-path programs/ssr-dvp-wrapper/Cargo.toml");
    println!("  solana program deploy target/deploy/ssr_compliance.so");
    println!("  solana program deploy target/deploy/ssr_dvp_wrapper.so");
    println!();

    let current_exe = std::env::current_exe()
        .with_context(|| "current_exe() failed".to_string())?;

    let mut report = EmbeddedReport {
        total: scenario.steps.len(),
        skipped: 0,
        passed: 0,
        failed: 0,
        expectations_unverified: 0,
    };

    for (i, step) in scenario.steps.iter().enumerate() {
        println!(
            "─── Step {} of {} ───────────────────────────",
            i + 1,
            scenario.steps.len()
        );
        println!("  {}", step.explanation);
        println!("  $ {}", step.command);
        if let Some(expect) = &step.expect {
            println!("  # (looking for: {expect})");
        }
        println!();

        let trimmed = step.command.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            println!("  (informational step — no command executed)");
            println!();
            report.skipped += 1;
            continue;
        }

        let has_shell_metas = has_shell_metacharacters(trimmed);

        let mut cmd = if has_shell_metas {
            let mut c = Command::new("sh");
            c.args(["-c", trimmed]);
            if let Some(exe_dir) = current_exe.parent() {
                let existing = std::env::var("PATH").unwrap_or_default();
                let new_path = format!("{}:{existing}", exe_dir.display());
                c.env("PATH", new_path);
            }
            c
        } else {
            let argv: Vec<&str> = trimmed.split_whitespace().collect();
            let (program, args) = match argv.split_first() {
                Some((p, a)) => (*p, a.to_vec()),
                None => continue,
            };
            let (cmd_name, cmd_args): (String, Vec<&str>) = if program == "ssr-cli" {
                (current_exe.to_string_lossy().into_owned(), args)
            } else {
                (program.to_string(), args)
            };
            let mut c = Command::new(&cmd_name);
            c.args(&cmd_args);
            c
        };

        let status = cmd.status();

        match status {
            Ok(s) if s.success() => {
                println!();
                println!("  ✓ step {} succeeded (exit 0)", i + 1);
                report.passed += 1;
                if step.expect.is_some() {
                    report.expectations_unverified += 1;
                }
            }
            Ok(s) => {
                println!();
                println!("  ✗ step {} exited {}", i + 1, s.code().unwrap_or(-1));
                report.failed += 1;
            }
            Err(e) => {
                println!();
                println!("  ✗ step {} failed to spawn: {e}", i + 1);
                if has_shell_metas {
                    println!("    (routed via `sh -c` because the command contains shell metacharacters; check that `sh` is available)");
                } else {
                    println!("    (ssr-cli prefixes auto-route to current_exe; other CLIs must be in PATH)");
                }
                report.failed += 1;
            }
        }
        println!();
    }

    println!("─── verdict ───────────────────────────────────────────");
    println!(
        "{} step(s): {} passed / {} failed / {} skipped (informational)",
        report.total, report.passed, report.failed, report.skipped
    );
    if report.expectations_unverified > 0 {
        println!(
            "{} step(s) declared expected-output substrings; v1 cannot verify these because it inherits stdio (v2 will tee).",
            report.expectations_unverified
        );
    }
    println!("source: {}", path.display());
    print!("{}", cta_footer());

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_scenario_json() -> &'static str {
        r#"{
            "name": "dvp-happy-path",
            "category": "dvp",
            "description": "Compliance bootstrap + DvP settle.",
            "headline": "Two verified participants atomically swap asset for cash via SSR-wrapped SPC DvP.",
            "steps": [
                {
                    "explanation": "Initialize the compliance registry.",
                    "command": "ssr-cli compliance init-registry",
                    "expect": "registry initialized"
                }
            ]
        }"#
    }

    #[test]
    fn scenario_round_trips() {
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        assert_eq!(s.name, "dvp-happy-path");
        assert_eq!(s.steps.len(), 1);
        assert_eq!(s.steps[0].expect.as_deref(), Some("registry initialized"));
    }

    #[test]
    fn render_list_contains_headline_and_cta() {
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        let path = PathBuf::from("/tmp/dvp.json");
        let out = render_list(&[(path, s)]);
        assert!(out.contains("dvp-happy-path"));
        assert!(out.contains("atomically swap"));
        assert!(out.contains("solana-prime-broker"));
        assert!(out.contains("NEXT:"));
    }

    #[test]
    fn render_show_contains_step_summary() {
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        let path = PathBuf::from("/tmp/dvp.json");
        let out = render_show(&s, &path);
        assert!(out.contains("steps       : 1 command(s)"));
        assert!(out.contains("Initialize the compliance registry"));
    }

    #[test]
    fn render_run_v0_includes_prereqs_and_steps() {
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        let path = PathBuf::from("scenarios/dvp-happy-path.json");
        let out = render_run_v0(&s, &path);
        assert!(out.contains("PREREQUISITES"));
        assert!(out.contains("solana-test-validator"));
        assert!(out.contains("Step 1 — Initialize the compliance registry."));
        assert!(out.contains("$ ssr-cli compliance init-registry"));
        assert!(out.contains("expect output to include: registry initialized"));
    }

    /// Regression tests for shell-metachar detection. The runner
    /// branches on this; both branches have failed in distinct ways
    /// in this session's history, so the detection is locked in here.

    #[test]
    fn metachar_routes_chains_through_sh() {
        assert!(has_shell_metacharacters("ssr-cli foo && ssr-cli bar"));
        assert!(has_shell_metacharacters("a || b"));
        assert!(has_shell_metacharacters("a; b"));
        assert!(has_shell_metacharacters("a | grep b"));
    }

    /// Regression: scenarios use `<PLACEHOLDER>` for operator-
    /// substituted values. Treating `<` / `>` as metachars would
    /// route every placeholder-bearing step through `sh -c`, where
    /// the placeholder is parsed as a stdin redirect (`<file`) and
    /// the step breaks. Locks the fix in.
    #[test]
    fn metachar_does_not_match_angle_bracket_placeholders() {
        assert!(!has_shell_metacharacters(
            "ssr-cli compliance register --participant <USER_A_PUBKEY> --jurisdiction JP"
        ));
        assert!(!has_shell_metacharacters("ssr-cli derive swap-dvp --user-a <USER_A>"));
        assert!(!has_shell_metacharacters("cmd <input> output"));
    }

    #[test]
    fn metachar_does_not_match_plain_commands() {
        assert!(!has_shell_metacharacters("ssr-cli compliance init-registry"));
        assert!(!has_shell_metacharacters(
            "ssr-cli margin show --user X --mint M1 --mint M2"
        ));
        assert!(!has_shell_metacharacters("solana program deploy x.so"));
    }

    /// v2 in-process dispatch tests.

    #[test]
    fn try_parse_in_process_matches_compliance_gate_demo() {
        assert!(matches!(
            try_parse_in_process("ssr-cli compliance-gate-demo"),
            Some(InProcessTarget::ComplianceGateDemo)
        ));
        assert!(matches!(
            try_parse_in_process("  ssr-cli compliance-gate-demo  "),
            Some(InProcessTarget::ComplianceGateDemo)
        ));
    }

    #[test]
    fn try_parse_in_process_returns_none_for_other_commands() {
        assert!(try_parse_in_process("ssr-cli compliance init-registry").is_none());
        assert!(try_parse_in_process("ssr-cli compliance-gate-demo --extra").is_none());
        assert!(try_parse_in_process("# comment").is_none());
        assert!(try_parse_in_process("spl-token create-token").is_none());
    }

    fn make_scenario(commands: &[&str]) -> Scenario {
        Scenario {
            name: "test".to_string(),
            category: "stress".to_string(),
            description: "test".to_string(),
            headline: "test".to_string(),
            steps: commands
                .iter()
                .map(|c| ScenarioStep {
                    explanation: "step".to_string(),
                    command: (*c).to_string(),
                    expect: None,
                })
                .collect(),
            expected_outcomes: Vec::new(),
        }
    }

    #[test]
    fn is_v2_eligible_true_when_all_steps_in_process() {
        let s = make_scenario(&["ssr-cli compliance-gate-demo"]);
        assert!(is_v2_eligible(&s));
    }

    #[test]
    fn is_v2_eligible_false_when_any_step_is_subprocess() {
        let s = make_scenario(&[
            "ssr-cli compliance-gate-demo",
            "ssr-cli compliance init-registry",
        ]);
        assert!(!is_v2_eligible(&s));
    }

    #[test]
    fn is_v2_eligible_false_for_empty_scenario() {
        let s = make_scenario(&[]);
        assert!(!is_v2_eligible(&s));
    }

    /// Phase 3: expected_outcomes parsing + evaluation.

    fn demo_result() -> crate::compliance_demo::ComplianceDemoResult {
        crate::compliance_demo::run_compliance_demo_structured()
    }

    #[test]
    fn evaluate_compliance_check_counts() {
        let r = demo_result();
        assert!(matches!(
            evaluate_compliance_check(&ComplianceCheck::AllowedPairs(2), &r),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_compliance_check(&ComplianceCheck::RejectedPairs(18), &r),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_compliance_check(&ComplianceCheck::AllowedPairs(0), &r),
            OutcomeStatus::Fail(_)
        ));
    }

    #[test]
    fn evaluate_compliance_check_named_pair() {
        let r = demo_result();
        assert!(matches!(
            evaluate_compliance_check(
                &ComplianceCheck::AllowedPair {
                    sender: "alice".to_string(),
                    receiver: "eve".to_string(),
                },
                &r
            ),
            OutcomeStatus::Pass
        ));
        // alice → bob: bob is SUSPENDED, so rejected
        assert!(matches!(
            evaluate_compliance_check(
                &ComplianceCheck::AllowedPair {
                    sender: "alice".to_string(),
                    receiver: "bob".to_string(),
                },
                &r
            ),
            OutcomeStatus::Fail(_)
        ));
        // bob → alice: bob is SUSPENDED, so rejected (this matches RejectedPair)
        assert!(matches!(
            evaluate_compliance_check(
                &ComplianceCheck::RejectedPair {
                    sender: "bob".to_string(),
                    receiver: "alice".to_string(),
                },
                &r
            ),
            OutcomeStatus::Pass
        ));
    }

    #[test]
    fn evaluate_compliance_check_reject_reason() {
        let r = demo_result();
        assert!(matches!(
            evaluate_compliance_check(
                &ComplianceCheck::RejectReason {
                    sender: "bob".to_string(),
                    receiver: "alice".to_string(),
                    reason_contains: "SUSPENDED".to_string(),
                },
                &r
            ),
            OutcomeStatus::Pass
        ));
        // Wrong substring → Fail
        assert!(matches!(
            evaluate_compliance_check(
                &ComplianceCheck::RejectReason {
                    sender: "bob".to_string(),
                    receiver: "alice".to_string(),
                    reason_contains: "BLOCKED".to_string(),
                },
                &r
            ),
            OutcomeStatus::Fail(_)
        ));
    }

    #[test]
    fn evaluate_compliance_check_unknown_name_fails() {
        let r = demo_result();
        let status = evaluate_compliance_check(
            &ComplianceCheck::AllowedPair {
                sender: "nonexistent".to_string(),
                receiver: "alice".to_string(),
            },
            &r,
        );
        match status {
            OutcomeStatus::Fail(why) => assert!(why.contains("not found")),
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn scenario_with_expected_outcomes_round_trips() {
        let json = r#"{
            "name": "test",
            "category": "stress",
            "description": "test",
            "headline": "test",
            "steps": [
                {"explanation": "step", "command": "ssr-cli compliance-gate-demo"}
            ],
            "expected_outcomes": [
                {
                    "name": "two-allowed",
                    "description": "two allowed",
                    "check": {"allowed_pairs": 2}
                },
                {
                    "name": "specific-pair",
                    "description": "alice→eve allowed",
                    "check": {"allowed_pair": {"sender": "alice", "receiver": "eve"}}
                },
                {
                    "name": "with-reason",
                    "description": "reject reason mentions SUSPENDED",
                    "check": {"reject_reason": {"sender": "bob", "receiver": "alice", "reason_contains": "SUSPENDED"}}
                }
            ]
        }"#;
        let s: Scenario = serde_json::from_str(json).expect("parse");
        assert_eq!(s.expected_outcomes.len(), 3);
        assert!(matches!(
            s.expected_outcomes[0].check,
            ComplianceCheck::AllowedPairs(2)
        ));
    }

    #[test]
    fn scenario_without_expected_outcomes_still_parses() {
        let json = r#"{
            "name": "test",
            "category": "stress",
            "description": "test",
            "headline": "test",
            "steps": [
                {"explanation": "step", "command": "ssr-cli compliance-gate-demo"}
            ]
        }"#;
        let s: Scenario = serde_json::from_str(json).expect("parse");
        assert!(s.expected_outcomes.is_empty());
    }
}
