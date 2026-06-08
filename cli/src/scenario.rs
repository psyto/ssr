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
}

/// Try to parse a step's command into an in-process target. Returns
/// `None` for commands that must still spawn a sub-process.
pub fn try_parse_in_process(command: &str) -> Option<InProcessTarget> {
    let trimmed = command.trim();
    if trimmed == "ssr-cli compliance-gate-demo" {
        return Some(InProcessTarget::ComplianceGateDemo);
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
                let _ = step; // explanation echoed by the renderer
                let _ = &mut failed; // no fallible path here today
            }
        }
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
    // HEADLINE — for now echo scenario.headline (Phase 3 will verify it).
    println!("HEADLINE: {}", scenario.headline);
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
        }
    }
    println!();

    // DELTA — the matrix itself.
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
        }
    }
    println!();

    // OUTCOMES — placeholder until Phase 3.
    println!("OUTCOMES:");
    println!(
        "  (no expected_outcomes declared — Phase 3 will add JSON-driven verification)"
    );
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
}
