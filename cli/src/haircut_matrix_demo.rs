//! Standalone haircut-matrix demo.
//!
//! Enumerates the 8 known SSR asset classes (TOKENIZED_DEPOSIT,
//! STABLECOIN, SOVEREIGN_BOND, …) and renders the default haircut
//! table plus the haircut-adjusted collateral value at three sample
//! notional sizes. Pure Rust against `ssr-types` constants — no
//! Solana validator, no LiteSVM, no deployed program required.
//!
//! Surfaces the engine's risk-tier model in one screen so a buyer
//! evaluating the prime-broker layer can see at a glance: which
//! asset classes the engine recognises, what default haircut each
//! carries, and what that means in dollar terms for a representative
//! collateral position.
//!
//! Same shape pattern as the existing `compliance_demo`: pure-compute
//! function returns a structured result; printed wrapper formats for
//! the CLI subcommand; the scenario runner consumes the structured
//! version directly to render the v2 5-section output contract.

use ssr_types::{asset_class, asset_class_label, default_haircut_bps};

#[derive(Debug, Clone)]
pub struct HaircutEntry {
    pub class: u8,
    pub label: String,
    pub haircut_bps: u16,
}

#[derive(Debug, Clone)]
pub struct HaircutMatrixDemoResult {
    pub entries: Vec<HaircutEntry>,
    /// Sample notional sizes used to populate the per-class
    /// effective-collateral grid.
    pub sample_notionals: Vec<u128>,
}

impl HaircutMatrixDemoResult {
    #[must_use]
    pub fn class_count(&self) -> usize {
        self.entries.len()
    }

    /// Compute haircut-adjusted collateral value for a given class and
    /// notional. Mirror of the on-chain math: `notional × (10_000 −
    /// haircut_bps) / 10_000`.
    #[must_use]
    pub fn effective_value(&self, class_idx: usize, notional: u128) -> u128 {
        let haircut = self.entries.get(class_idx).map(|e| e.haircut_bps).unwrap_or(10_000);
        notional.saturating_mul(u128::from(10_000_u16.saturating_sub(haircut))) / 10_000
    }

    /// Lookup by class label, e.g. "EQUITY". Returns None if absent.
    #[must_use]
    pub fn find_class(&self, label: &str) -> Option<&HaircutEntry> {
        self.entries.iter().find(|e| e.label == label)
    }

    /// True iff haircuts are non-decreasing when the entries appear
    /// in the order produced by `run_haircut_matrix_demo_structured`
    /// (deliberately ordered safest-first → riskiest-last).
    #[must_use]
    pub fn haircuts_monotonic_non_decreasing(&self) -> bool {
        self.entries.windows(2).all(|w| w[1].haircut_bps >= w[0].haircut_bps)
    }
}

fn entry(class: u8) -> HaircutEntry {
    HaircutEntry {
        class,
        label: asset_class_label(class).to_string(),
        haircut_bps: default_haircut_bps(class),
    }
}

/// Pure-compute version. Returns the structured matrix.
///
/// Entries are ordered safest-first (ascending haircut) so the
/// monotonic check + visual sweep both read naturally.
#[must_use]
pub fn run_haircut_matrix_demo_structured() -> HaircutMatrixDemoResult {
    let entries = vec![
        entry(asset_class::TOKENIZED_DEPOSIT), //    0 bps
        entry(asset_class::STABLECOIN),        //    0 bps
        entry(asset_class::SOVEREIGN_BOND),    //  500 bps
        entry(asset_class::CORPORATE_BOND),    // 1500 bps
        entry(asset_class::FUND_UNIT),         // 2000 bps
        entry(asset_class::COMMODITY),         // 2500 bps
        entry(asset_class::EQUITY),            // 3000 bps
        entry(asset_class::REAL_ASSET),        // 4000 bps
    ];
    let sample_notionals = vec![10_000_u128, 100_000_u128, 1_000_000_u128];
    HaircutMatrixDemoResult { entries, sample_notionals }
}

/// CLI subcommand body. Prints the matrix in a human-readable form.
pub fn run_haircut_matrix_demo_cli() {
    let r = run_haircut_matrix_demo_structured();

    println!();
    println!("=== ssr — haircut matrix demo ===");
    println!();
    println!("    Every asset class the SSR prime-broker engine recognises carries");
    println!("    a default haircut (governance-mutable at deployment time). For a");
    println!("    representative notional, the haircut tells you the collateral");
    println!("    value the cross-margin engine actually credits.");
    println!();

    println!("    Asset class ordered safest → riskiest (by haircut):");
    println!();
    println!(
        "      {:<18}  {:<8}  {:>12}  {:>12}  {:>14}",
        "Class", "Haircut", "10K notional", "100K notional", "1M notional"
    );
    println!(
        "      {:<18}  {:<8}  {:>12}  {:>12}  {:>14}",
        "─".repeat(18),
        "─".repeat(8),
        "─".repeat(12),
        "─".repeat(12),
        "─".repeat(14)
    );
    for (i, e) in r.entries.iter().enumerate() {
        let pct = e.haircut_bps as f64 / 100.0;
        print!(
            "      {:<18}  {:>5.1}%   ",
            e.label, pct
        );
        for &notional in &r.sample_notionals {
            let eff = r.effective_value(i, notional);
            print!(" {:>12}", eff);
        }
        println!();
    }
    println!();

    println!("    Engineering interpretation:");
    println!("      • Stablecoins + tokenised deposits credit at full notional (0% haircut).");
    println!("      • Sovereign bonds get a small (5%) discount — assumed near-cash.");
    println!("      • Equity-class collateral loses ~30% — covering both price-impact");
    println!("        risk and liquidation friction.");
    println!("      • Real assets carry the heaviest standing haircut (40%) — slow to");
    println!("        liquidate and prone to wide bid-ask gaps.");
    println!();
    println!("    Governance can tighten any class via `compliance set-haircut`; this");
    println!("    table is just the deployment-time default.");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_known_asset_classes() {
        let r = run_haircut_matrix_demo_structured();
        assert_eq!(r.class_count(), 8);
    }

    #[test]
    fn tokenized_deposit_and_stablecoin_are_zero_haircut() {
        let r = run_haircut_matrix_demo_structured();
        let td = r.find_class("TOKENIZED_DEPOSIT").expect("present");
        let sc = r.find_class("STABLECOIN").expect("present");
        assert_eq!(td.haircut_bps, 0);
        assert_eq!(sc.haircut_bps, 0);
    }

    #[test]
    fn real_asset_carries_the_heaviest_default_haircut() {
        let r = run_haircut_matrix_demo_structured();
        let ra = r.find_class("REAL_ASSET").expect("present");
        let max_haircut = r.entries.iter().map(|e| e.haircut_bps).max().unwrap();
        assert_eq!(ra.haircut_bps, max_haircut);
        assert_eq!(ra.haircut_bps, 4_000);
    }

    #[test]
    fn equity_default_is_thirty_percent() {
        let r = run_haircut_matrix_demo_structured();
        let eq = r.find_class("EQUITY").expect("present");
        assert_eq!(eq.haircut_bps, 3_000);
    }

    #[test]
    fn haircut_ordering_is_monotonic_non_decreasing() {
        let r = run_haircut_matrix_demo_structured();
        assert!(r.haircuts_monotonic_non_decreasing());
    }

    #[test]
    fn effective_value_at_zero_haircut_is_full_notional() {
        let r = run_haircut_matrix_demo_structured();
        // TOKENIZED_DEPOSIT is at index 0.
        assert_eq!(r.effective_value(0, 100_000), 100_000);
    }

    #[test]
    fn effective_value_at_thirty_percent_haircut() {
        let r = run_haircut_matrix_demo_structured();
        // EQUITY is at index 6 in the safest-first ordering.
        let eq_idx = r.entries.iter().position(|e| e.label == "EQUITY").unwrap();
        // 100_000 × (10_000 − 3_000) / 10_000 = 70_000
        assert_eq!(r.effective_value(eq_idx, 100_000), 70_000);
    }
}
