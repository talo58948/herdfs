//! F8: Jev failures, per-setting uncertainty/malformed answers, and remembered plans.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use herdfs_lab::{JevAnswers, JevMode, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, assert_reads, commit, random_bytes, start, wait_until};

const FILE_LEN: usize = 256 << 10;
const DECISION_BUDGET: Duration = Duration::from_millis(300);
const SLOW: Duration = Duration::from_secs(5);
const STABLE_CONTEXT: Profile = Profile::Custom {
    round_trip_ms: 200,
    kilobytes_per_sec: 5_000,
    store_first_byte_ms: 0,
};

fn config() -> LabConfig {
    LabConfig::default()
        .client("w", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("jev", "jev", &[], STABLE_CONTEXT, Strategy::Jev)
        .client("rule", "rule", &[], STABLE_CONTEXT, Strategy::Rule)
}

#[test]
fn reads_keep_their_latency_and_bytes_while_jev_is_slow_or_down() -> Result<()> {
    // contract: P1, P2, C8
    let Some(lab) = start(config())? else {
        return Ok(());
    };
    for (errors, mode) in [(3, JevMode::Slow(SLOW)), (6, JevMode::Down)] {
        lab.jev("jev").set(mode);
        let (name, bytes) = (format!("f8-{errors}.bin"), random_bytes(FILE_LEN));
        commit(&lab, "w", &name, &bytes)?;
        thread::sleep(VISIBLE_AFTER);
        let read = |client| -> Result<Duration> {
            let started = Instant::now();
            assert_reads(&lab, client, &name, &bytes)?;
            Ok(started.elapsed())
        };
        let (by_rule, by_jev) = (read("rule")?, read("jev")?);
        assert!(
            by_jev < by_rule + DECISION_BUDGET,
            "{mode:?}: {by_jev:?} versus {by_rule:?}"
        );
        wait_until("failed settings are counted", SLOW, || {
            Ok(lab.stats("jev")?["jev_errors"] == errors)
        })?;
    }
    Ok(())
}

#[test]
fn uncertainty_falls_back_for_only_that_setting_and_is_remembered() -> Result<()> {
    // contract: P1, P2, P3
    let Some(lab) = start(config())? else {
        return Ok(());
    };
    lab.jev("jev").set(JevMode::Answers(JevAnswers {
        prefetch: ("index", 0.9),
        read_ahead: ("large", 0.1),
        keep: ("yes", 0.9),
    }));
    let (first, second) = (random_bytes(FILE_LEN), random_bytes(FILE_LEN));
    commit(&lab, "w", "partial-a.bin", &first)?;
    commit(&lab, "w", "partial-b.bin", &second)?;
    thread::sleep(VISIBLE_AFTER);
    assert_reads(&lab, "jev", "partial-a.bin", &first)?;
    wait_until("three Jev settings settle", SLOW, || {
        let stats = lab.stats("jev")?;
        Ok(stats["jev_answers"] == 2 && stats["jev_unsure"] == 1)
    })?;
    assert_reads(&lab, "jev", "partial-b.bin", &second)?;
    let plan = lab.plan("jev", "partial-b.bin".as_ref())?;
    assert_eq!(
        plan["plan"],
        serde_json::json!({"prefetch":"index", "read_ahead":"none", "keep":"yes"})
    );
    assert_eq!(plan["source"], "remembered");
    assert_eq!(lab.jev("jev").requests(), 1);
    Ok(())
}

#[test]
fn one_malformed_answer_falls_back_without_losing_the_other_settings() -> Result<()> {
    // contract: P2, P3
    let Some(lab) = start(config())? else {
        return Ok(());
    };
    let cases = [
        (
            "prefetch",
            "p",
            serde_json::json!({"prefetch":"whole", "read_ahead":"large", "keep":"yes"}),
        ),
        (
            "read_ahead",
            "r",
            serde_json::json!({"prefetch":"index", "read_ahead":"none", "keep":"yes"}),
        ),
        (
            "keep",
            "k",
            serde_json::json!({"prefetch":"index", "read_ahead":"large", "keep":"no"}),
        ),
    ];
    for (index, (setting, extension, expected)) in cases.into_iter().enumerate() {
        lab.jev("jev").set(JevMode::Malformed(setting));
        let first_bytes = random_bytes(FILE_LEN);
        let second_bytes = random_bytes(FILE_LEN);
        let first = format!("bad-a.{extension}");
        let second = format!("bad-b.{extension}");
        commit(&lab, "w", &first, &first_bytes)?;
        commit(&lab, "w", &second, &second_bytes)?;
        thread::sleep(VISIBLE_AFTER);
        assert_reads(&lab, "jev", &first, &first_bytes)?;
        wait_until("the malformed setting is counted", SLOW, || {
            Ok(lab.stats("jev")?["jev_errors"] == index + 1)
        })?;
        assert_reads(&lab, "jev", &second, &second_bytes)?;
        assert_eq!(lab.plan("jev", second.as_ref())?["plan"], expected);
    }
    Ok(())
}
