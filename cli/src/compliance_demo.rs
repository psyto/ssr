//! Standalone compliance-gate behavior demo.
//!
//! Builds a small population of participants with different compliance
//! statuses (`VERIFIED` / `PENDING` / `SUSPENDED` / `BLOCKED`) and
//! exhaustively evaluates whether each (sender, receiver) pair would
//! be allowed to transfer through the on-chain compliance gate.
//!
//! Pure Rust against `ssr-types` primitives — no Solana validator, no
//! LiteSVM, no deployed programs. The point is to make the gate's
//! decision matrix legible to a non-engineer buyer in seconds:
//!
//! ```text
//!                 → alice    bob       carol     dan
//!   alice (V)     —          reject    reject    reject
//!   bob   (S)     reject     —         reject    reject
//!   carol (P)     reject     reject    —         reject
//!   dan   (B)     reject     reject    reject    —
//! ```
//!
//! Only `VERIFIED ↔ VERIFIED` transfers clear the gate; every other
//! pair is rejected, with the specific reason ("sender SUSPENDED",
//! "receiver PENDING", etc) surfaced for triage.
//!
//! This module is wrapped by the `ssr-cli compliance-gate-demo`
//! subcommand (printed form) and by the `scenario run` v2 path
//! (structured form, rendered as the scenario's DELTA section).

use ssr_types::{compliance_status, AccountRecord, CheckError};

/// One synthetic participant in the demo. The `name` is a label
/// (`alice` / `bob` / …); `status` is the actual `compliance_status::*`
/// constant the gate evaluates against.
#[derive(Debug, Clone)]
pub struct ParticipantSim {
    pub name: String,
    pub status: u8,
    pub status_label: &'static str,
}

/// One row of the (sender, receiver) outcome matrix.
#[derive(Debug, Clone)]
pub struct TransferOutcome {
    pub sender_idx: usize,
    pub receiver_idx: usize,
    pub allowed: bool,
    /// Plain-English reason (`"ok (both VERIFIED)"` or
    /// `"sender SUSPENDED"` or …) — what a non-engineer can read.
    pub reason: String,
}

/// Full result of one demo run.
#[derive(Debug, Clone)]
pub struct ComplianceDemoResult {
    pub participants: Vec<ParticipantSim>,
    pub matrix: Vec<TransferOutcome>,
}

impl ComplianceDemoResult {
    /// Number of (sender, receiver) pairs evaluated. Excludes self-
    /// transfers (i == j).
    #[must_use]
    pub fn total_pairs(&self) -> usize {
        self.matrix.len()
    }

    /// Number of pairs the gate allowed.
    #[must_use]
    pub fn allowed_pairs(&self) -> usize {
        self.matrix.iter().filter(|t| t.allowed).count()
    }

    /// Number of pairs the gate rejected.
    #[must_use]
    pub fn rejected_pairs(&self) -> usize {
        self.matrix.iter().filter(|t| !t.allowed).count()
    }
}

/// Convenience: short label for each status used in the printed and
/// rendered output.
fn label_for(status: u8) -> &'static str {
    match status {
        compliance_status::PENDING => "PENDING",
        compliance_status::VERIFIED => "VERIFIED",
        compliance_status::SUSPENDED => "SUSPENDED",
        compliance_status::BLOCKED => "BLOCKED",
        _ => "UNKNOWN",
    }
}

fn make_record(participant_seed: u8, status: u8) -> AccountRecord {
    // Deterministic synthetic pubkey from the seed byte. The compliance
    // gate doesn't care about the pubkey value when the record is the
    // one being checked (it only consults `status`), but the field has
    // to be present.
    let mut participant = [0u8; 32];
    participant[0] = participant_seed;
    AccountRecord {
        updated_at_slot: 1,
        participant,
        status,
        jurisdiction: *b"JP",
        flags: 0,
        bump: 254,
        _reserved: [0; 11],
    }
}

fn reason_for_block(role: &str, err: CheckError) -> String {
    match err {
        CheckError::Suspended => format!("{role} SUSPENDED"),
        CheckError::Blocked => format!("{role} BLOCKED"),
        CheckError::Unverified => format!("{role} not VERIFIED"),
        CheckError::StatusUnknown => format!("{role} status unknown"),
        CheckError::LayoutInvalid => format!("{role} layout invalid"),
    }
}

/// Pure-compute version. Returns the structured matrix without
/// printing.
#[must_use]
pub fn run_compliance_demo_structured() -> ComplianceDemoResult {
    let participants = vec![
        ParticipantSim {
            name: "alice".to_string(),
            status: compliance_status::VERIFIED,
            status_label: label_for(compliance_status::VERIFIED),
        },
        ParticipantSim {
            name: "bob".to_string(),
            status: compliance_status::SUSPENDED,
            status_label: label_for(compliance_status::SUSPENDED),
        },
        ParticipantSim {
            name: "carol".to_string(),
            status: compliance_status::PENDING,
            status_label: label_for(compliance_status::PENDING),
        },
        ParticipantSim {
            name: "dan".to_string(),
            status: compliance_status::BLOCKED,
            status_label: label_for(compliance_status::BLOCKED),
        },
        ParticipantSim {
            name: "eve".to_string(),
            status: compliance_status::VERIFIED,
            status_label: label_for(compliance_status::VERIFIED),
        },
    ];

    let mut matrix = Vec::new();
    for (i, sender) in participants.iter().enumerate() {
        for (j, receiver) in participants.iter().enumerate() {
            if i == j {
                continue;
            }
            let sender_rec = make_record(i as u8 + 1, sender.status);
            let receiver_rec = make_record(j as u8 + 1, receiver.status);

            let s_ok = sender_rec.check_transfer_allowed();
            let r_ok = receiver_rec.check_transfer_allowed();
            let (allowed, reason) = match (s_ok, r_ok) {
                (Ok(()), Ok(())) => (true, "ok (both VERIFIED)".to_string()),
                (Err(e), _) => (false, reason_for_block("sender", e)),
                (_, Err(e)) => (false, reason_for_block("receiver", e)),
            };
            matrix.push(TransferOutcome {
                sender_idx: i,
                receiver_idx: j,
                allowed,
                reason,
            });
        }
    }

    ComplianceDemoResult { participants, matrix }
}

/// CLI subcommand body. Prints the matrix in a human-readable form.
pub fn run_compliance_demo_cli() {
    let r = run_compliance_demo_structured();

    println!();
    println!("=== ssr — compliance gate demo ===");
    println!();
    println!("    Four synthetic participants are seeded with one of the four");
    println!("    compliance statuses (VERIFIED / PENDING / SUSPENDED / BLOCKED).");
    println!("    Every (sender, receiver) pair is evaluated against the gate.");
    println!();

    println!("    Participants:");
    for p in &r.participants {
        println!("      {:<7} → {}", p.name, p.status_label);
    }
    println!();

    println!("    Transfer-allowed matrix (row = sender, column = receiver):");
    print!("      {:<8}", "");
    for p in &r.participants {
        print!("  {:<10}", p.name);
    }
    println!();
    for (i, sender) in r.participants.iter().enumerate() {
        print!("      {:<8}", sender.name);
        for j in 0..r.participants.len() {
            if i == j {
                print!("  {:<10}", "—");
                continue;
            }
            let outcome = r
                .matrix
                .iter()
                .find(|t| t.sender_idx == i && t.receiver_idx == j)
                .expect("matrix is exhaustive");
            print!("  {:<10}", if outcome.allowed { "allow" } else { "reject" });
        }
        println!();
    }
    println!();

    println!(
        "    Verdict: {} of {} pair(s) allowed, {} rejected.",
        r.allowed_pairs(),
        r.total_pairs(),
        r.rejected_pairs()
    );
    println!("    Only VERIFIED ↔ VERIFIED transfers clear the gate. Every other");
    println!("    pair carries an explicit failure reason — operators see WHY a");
    println!("    transfer rejected, not a generic error.");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_verified_to_verified_pairs_are_allowed() {
        let r = run_compliance_demo_structured();
        // 5 participants × 5 receivers − 5 self pairs = 20 pairs.
        assert_eq!(r.total_pairs(), 20);
        // alice and eve are the two VERIFIED participants → 2 allowed
        // pairs (alice→eve and eve→alice).
        assert_eq!(r.allowed_pairs(), 2);
        assert_eq!(r.rejected_pairs(), 18);
    }

    #[test]
    fn allowed_pairs_are_specifically_verified_to_verified() {
        let r = run_compliance_demo_structured();
        for outcome in r.matrix.iter().filter(|t| t.allowed) {
            let sender = &r.participants[outcome.sender_idx];
            let receiver = &r.participants[outcome.receiver_idx];
            assert_eq!(sender.status_label, "VERIFIED");
            assert_eq!(receiver.status_label, "VERIFIED");
        }
    }

    #[test]
    fn participants_have_distinct_names() {
        let r = run_compliance_demo_structured();
        let names: std::collections::HashSet<&str> =
            r.participants.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names.len(), r.participants.len());
    }

    #[test]
    fn each_outcome_has_specific_reason() {
        let r = run_compliance_demo_structured();
        for outcome in &r.matrix {
            assert!(
                outcome.reason.contains("VERIFIED")
                    || outcome.reason.contains("SUSPENDED")
                    || outcome.reason.contains("BLOCKED")
                    || outcome.reason.contains("PENDING")
                    || outcome.reason.contains("not VERIFIED"),
                "outcome {outcome:?} has unrecognised reason"
            );
        }
    }

    #[test]
    fn all_four_statuses_are_represented() {
        let r = run_compliance_demo_structured();
        let statuses: std::collections::HashSet<&'static str> =
            r.participants.iter().map(|p| p.status_label).collect();
        assert!(statuses.contains("VERIFIED"));
        assert!(statuses.contains("SUSPENDED"));
        assert!(statuses.contains("PENDING"));
        assert!(statuses.contains("BLOCKED"));
    }
}
