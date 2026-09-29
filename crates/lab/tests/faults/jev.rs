//! F8: Jev slow or down, and Jev's answers remembered. Opens never wait for Jev, so reads keep
//! their latency and bytes, and each failed question is counted.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use herdfs_lab::{JevMode, LabConfig, Profile, Strategy};

use crate::{VISIBLE_AFTER, assert_reads, commit, random_bytes, start, wait_until};

/// Small enough that the rule fetches it whole, as Jev's fallback does.
const FILE_LEN: usize = 256 << 10;
/// The decision budget of P1.
const DECISION_BUDGET: Duration = Duration::from_millis(300);
/// Longer than the client waits for Jev.
const SLOW: Duration = Duration::from_secs(5);
/// Keeps Jev's round-trip and download-time phrases stable under build-box load.
const STABLE_CONTEXT: Profile = Profile::Custom {
    round_trip_ms: 200,
    kilobytes_per_sec: 5_000,
    store_first_byte_ms: 0,
};

fn writer_and(readers: &[(&str, Strategy)]) -> LabConfig {
    let config = LabConfig::default().client("w", "alice", &[], Profile::Lan, Strategy::Stream);
    readers.iter().fold(config, |config, &(name, strategy)| {
        config.client(name, name, &[], Profile::Lan, strategy)
    })
}

#[test]
fn reads_keep_their_latency_and_bytes_while_jev_is_slow_or_down() -> Result<()> {
    // contract: P1, P2
    let readers = [("jev", Strategy::Jev), ("rule", Strategy::Rule)];
    let Some(lab) = start(writer_and(&readers))? else {
        return Ok(());
    };
    for (failures, mode) in [(1, JevMode::Slow(SLOW)), (2, JevMode::Down)] {
        lab.jev("jev").set(mode);
        let (name, bytes) = (format!("f8-{failures}.bin"), random_bytes(FILE_LEN));
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
            "{mode:?}: the read took {by_jev:?} with Jev, {by_rule:?} with the rule"
        );
        wait_until("the failed question is counted", SLOW, || {
            Ok(lab.stats("jev")?["jev_errors"] == failures)
        })?;
    }
    Ok(())
}

#[test]
fn a_second_open_of_the_same_context_takes_jevs_remembered_answer() -> Result<()> {
    // contract: P1
    let config = LabConfig::default()
        .client("w", "alice", &[], Profile::Lan, Strategy::Stream)
        .client("jev", "jev", &[], STABLE_CONTEXT, Strategy::Jev);
    let Some(lab) = start(config)? else {
        return Ok(());
    };
    // The rule would fetch files this small, so a pin can only come from Jev.
    lab.jev("jev").set(JevMode::Answer("pin", 0.9));
    let (first, second) = (random_bytes(FILE_LEN), random_bytes(FILE_LEN));
    commit(&lab, "w", "first.bin", &first)?;
    commit(&lab, "w", "second.bin", &second)?;
    thread::sleep(VISIBLE_AFTER);

    assert_reads(&lab, "jev", "first.bin", &first)?;
    wait_until("Jev's answer arrives", SLOW, || {
        Ok(lab.stats("jev")?["jev_answers"] == 1)
    })?;
    assert_reads(&lab, "jev", "second.bin", &second)?;
    let stats = lab.stats("jev")?;
    assert_eq!(lab.jev("jev").questions(), 1, "the second open asks nothing");
    assert_eq!(stats["decisions"]["pin"], 1, "{stats}");
    Ok(())
}
