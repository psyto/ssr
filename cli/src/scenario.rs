//! Sandbox scenario surface for ssr-cli.
//!
//! A *scenario* is a metadata-wrapped recipe of `ssr-cli` invocations
//! that demonstrate one institutional Solana prime broker behavior
//! end-to-end. Per `fabrknt/website/SANDBOX-PATTERN.md`, this surface
//! implements: (1) pre-baked scenarios in `scenarios/*.json`, (2) ASCII
//! headline + step rendering, (3) parameter dial via per-step args,
//! (4) replay (the JSON IS the replay format), (5) CTA footer.
//!
//! v0 prints the step list with explanations rather than executing in
//! sub-processes. Embedded execution + state-diff rendering lands in
//! v1.

use std::fs;
use std::path::{Path, PathBuf};

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

fn cta_footer() -> String {
    let mut out = String::new();
    out.push_str("\nNEXT:\n");
    out.push_str("  • Adopt this engine  : https://github.com/psyto/ssr\n");
    out.push_str("  • Custom build       : https://fabrknt.com/waitlist.html?product=solana-prime-broker&intent=build\n");
    out.push_str("  • Hosted access      : https://fabrknt.com/waitlist.html?product=solana-prime-broker&intent=hosted\n");
    out
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
}
