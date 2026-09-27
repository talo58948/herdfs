//! pjdfstest, the POSIX conformance suite, on a lab mount. Every test must pass or be skipped,
//! except those in [`OUT_OF_V0`], which need a feature deliberately left out of v0, and those in
//! [`BUGS`], each reproduced by an ignored test in `bugs.rs`. A listed test that stops failing
//! fails the suite, so the lists stay honest.

mod bugs;
mod out_of_v0;

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};
use herdfs_lab::{Lab, LabConfig, Profile, Strategy};

use crate::bugs::BUGS;
use crate::out_of_v0::OUT_OF_V0;

/// One test in pjdfstest's report: its name, `ok`, `skipped` or `FAILED`, and the lines after it.
struct Outcome<'a> {
    name: &'a str,
    status: &'a str,
    detail: Vec<&'a str>,
}

#[test]
fn posix_conformance_except_features_out_of_v0() -> Result<()> {
    // contract: C1, C7
    let Some(lab) = lab()? else {
        return Ok(());
    };
    let report = run_pjdfstest(lab.mount_path("a"))?;
    let summary = report.lines().find(|line| line.starts_with("Summary:"));
    let summary = summary.with_context(|| format!("pjdfstest did not finish:\n{report}"))?;
    let outcomes = outcomes(&report);
    let failed = |name: &str| {
        outcomes
            .iter()
            .any(|outcome| outcome.name == name && outcome.status == "FAILED")
    };
    let expected: Vec<(&str, &str)> = (OUT_OF_V0.iter().chain(BUGS))
        .flat_map(|(why, names)| names.iter().map(move |name| (*name, *why)))
        .collect();
    let unexpected: Vec<String> = outcomes
        .iter()
        .filter(|outcome| outcome.status == "FAILED")
        .filter(|outcome| expected.iter().all(|(name, _)| *name != outcome.name))
        .map(|outcome| format!("{}\n\t{}", outcome.name, outcome.detail.join("\n\t")))
        .collect();
    let not_failing: Vec<String> = (expected.iter())
        .filter(|(name, _)| !failed(name))
        .map(|(name, why)| format!("{name} ({why})"))
        .collect();
    eprintln!("pjdfstest {summary}; {} expected", expected.len());
    ensure!(
        unexpected.is_empty() && not_failing.is_empty(),
        "unexpected failures:\n{}\n\nlisted as failing but did not fail:\n{}",
        unexpected.join("\n"),
        not_failing.join("\n"),
    );
    Ok(())
}

/// A lab whose client `a` mounts as alice, or `None` if the binaries are not built.
fn lab() -> Result<Option<Lab>> {
    if let Some(reason) = herdfs_lab::missing_binaries() {
        eprintln!("skipped: {reason}");
        return Ok(None);
    }
    let config = LabConfig::default().client("a", "alice", &[], Profile::Lan, Strategy::Stream);
    Ok(Some(Lab::start(config)?))
}

/// Runs pjdfstest in `dir` with the lab's settings and returns its report.
fn run_pjdfstest(dir: &Path) -> Result<String> {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("pjdfstest.toml");
    let output = Command::new("pjdfstest")
        .arg("-c")
        .arg(config)
        .arg("-p")
        .arg(dir)
        .env("NO_COLOR", "1")
        .output()
        .context("running pjdfstest; install it with scripts/install-test-tools")?;
    Ok(String::from_utf8(output.stdout)?)
}

fn outcomes(report: &str) -> Vec<Outcome<'_>> {
    let mut outcomes: Vec<Outcome> = Vec::new();
    for line in report.lines() {
        if let Some(detail) = line.strip_prefix('\t') {
            outcomes
                .last_mut()
                .into_iter()
                .for_each(|last| last.detail.push(detail));
        } else if let [name, status] = line.split_whitespace().collect::<Vec<_>>()[..] {
            let detail = Vec::new();
            outcomes.push(Outcome { name, status, detail });
        }
    }
    outcomes
}
