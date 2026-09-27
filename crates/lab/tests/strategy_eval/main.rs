//! R5 (docs/verification.md): Jev against the fixed deciders and the rule, scored by regret
//! against the measured best strategy. Jev is asked about each case with the client's own
//! question. Reads the AI Gateway key from `~/.config/herdfs/ai-gateway-key`, and writes the
//! table to `results/strategy_eval.json`.
//! contract: none; R5 measures how well Jev decides, which P1 and P2 do not promise.

mod cases;

use std::path::Path;
use std::time::Instant;
use std::{env, fs};

use anyhow::{Context, Result, ensure};
use herdfs_client::{Jev, OpenContext, Strategy};
use herdfs_lab::Profile;
use serde_json::{Value, json};

use crate::cases::{Case, LINKS, MIB, Workload, measure_all};

const JEV_URL: &str = "https://ai-gateway.vercel.sh/v1/evaluate";

/// Jev's raw answer for a case (`None` if the ask failed), and how long it took.
struct Asked {
    answer: Option<(Strategy, f64)>,
    millis: u128,
}

#[test]
#[ignore = "R5: runs for about 15 minutes and asks Jev over the network"]
fn jev_beats_every_fixed_strategy_by_regret() -> Result<()> {
    let key_file = Path::new(&env::var("HOME")?).join(".config/herdfs/ai-gateway-key");
    let jev = Jev::new(&key_file, JEV_URL).with_context(|| format!("reading {}", key_file.display()))?;
    let cases = measure_all()?;
    let contexts: Vec<OpenContext> = cases
        .iter()
        .map(|case| context(case, throughput(&cases, case.link)))
        .collect();
    let asked = ask_all(&jev, &contexts)?;
    // As the client's decider does: an unsure or failed answer leaves the rule's choice.
    let by_jev = contexts
        .iter()
        .zip(&asked)
        .map(|(context, asked)| match asked.answer {
            Some((strategy, confidence)) if confidence >= Jev::CONFIDENT => strategy,
            _ => Strategy::by_rule(context),
        });
    let mut deciders: Vec<(&str, Vec<Strategy>)> = Strategy::ALL
        .into_iter()
        .map(|strategy| (strategy.name(), vec![strategy; cases.len()]))
        .collect();
    deciders.push(("rule", contexts.iter().map(Strategy::by_rule).collect()));
    deciders.push(("jev", by_jev.collect()));
    // Jev's answers whatever their confidence, to show what the threshold costs. Not asserted.
    let raw = contexts.iter().zip(&asked).map(|(context, asked)| {
        asked
            .answer
            .map_or(Strategy::by_rule(context), |(strategy, _)| strategy)
    });
    deciders.push(("jev-raw", raw.collect()));
    let scores: Vec<(f64, f64)> = deciders
        .iter()
        .map(|(_, choices)| score(&cases, choices))
        .collect();
    report(&cases, &asked, &deciders, &scores)?;

    let best_fixed = scores[..3]
        .iter()
        .map(|(mean, _)| *mean)
        .fold(f64::INFINITY, f64::min);
    let (mean, p90) = scores[4];
    ensure!(
        mean < best_fixed && p90 <= 1.5,
        "Jev's regret: mean {mean:.2} (the best fixed decider's is {best_fixed:.2}), p90 {p90:.2} (at most 1.5)"
    );
    Ok(())
}

/// The context the client would have at the case's first open, over a link of `throughput`.
fn context(case: &Case, throughput: f64) -> OpenContext {
    OpenContext {
        size: case.mib * MIB,
        extension: Some("bin".into()),
        comm: case.workload.comm().into(),
        coverage: 0.0,
        throughput,
    }
}

/// Asks Jev about each context in turn, timing each ask.
fn ask_all(jev: &Jev, contexts: &[OpenContext]) -> Result<Vec<Asked>> {
    let runtime = tokio::runtime::Runtime::new()?;
    let ask = |context| {
        let started = Instant::now();
        let answer = runtime.block_on(jev.ask(context));
        let millis = started.elapsed().as_millis();
        let answer = answer
            .inspect_err(|err| eprintln!("asking Jev failed: {err}"))
            .ok();
        Asked { answer, millis }
    };
    Ok(contexts.iter().map(ask).collect())
}

/// The link's throughput in bytes per second, as the Fetch runs of whole-file reads measured it.
fn throughput(cases: &[Case], link: Profile) -> f64 {
    let fetch = Strategy::Fetch as usize;
    let reads = cases
        .iter()
        .filter(|case| case.link == link && matches!(case.workload, Workload::Read));
    let (bytes, seconds) = reads.fold((0.0, 0.0), |(bytes, seconds), case| {
        (bytes + case.bytes[fetch] as f64, seconds + case.seconds[fetch])
    });
    bytes / seconds
}

/// Mean and p90 regret of `choices`, one per case.
fn score(cases: &[Case], choices: &[Strategy]) -> (f64, f64) {
    let mut regrets: Vec<f64> = cases
        .iter()
        .zip(choices)
        .map(|(case, &choice)| case.regret(choice))
        .collect();
    regrets.sort_by(f64::total_cmp);
    let mean = regrets.iter().sum::<f64>() / regrets.len() as f64;
    (mean, regrets[(regrets.len() * 9).div_ceil(10) - 1])
}

/// Prints the table and writes it to `results/strategy_eval.json`.
fn report(
    cases: &[Case],
    asked: &[Asked],
    deciders: &[(&str, Vec<Strategy>)],
    scores: &[(f64, f64)],
) -> Result<()> {
    println!(
        "{:<22} {:>7} {:>7} {:>7}  {:<7} {:<7} {:<7} jev's answer",
        "case", "stream", "fetch", "pin", "best", "rule", "jev"
    );
    let mut rows = Vec::new();
    for (index, (case, asked)) in cases.iter().zip(asked).enumerate() {
        let [stream, fetch, pin] = case.seconds;
        let decisions: Vec<&str> = deciders
            .iter()
            .map(|(_, choices)| choices[index].name())
            .collect();
        let answer = asked
            .answer
            .map(|(strategy, confidence)| (strategy.name(), confidence));
        let (best, name) = (case.best().name(), case.name());
        let said = answer.map_or("failed".into(), |(choice, confidence)| {
            format!("{choice} {confidence:.2}")
        });
        println!(
            "{name:<22} {stream:>7.2} {fetch:>7.2} {pin:>7.2}  {best:<7} {:<7} {:<7} {said} in {} ms",
            decisions[3], decisions[4], asked.millis
        );
        rows.push(json!({
            "case": name, "seconds": case.seconds, "bytes": case.bytes, "best": best,
            "rule": decisions[3], "jev": decisions[4], "jev_answer": answer, "jev_ms": asked.millis,
        }));
    }
    let mut regret = serde_json::Map::new();
    for ((name, _), (mean, p90)) in deciders.iter().zip(scores) {
        println!("{name:<7} regret: mean {mean:.2}, p90 {p90:.2}");
        regret.insert((*name).into(), json!({"mean": mean, "p90": p90}));
    }
    let links: Value = LINKS
        .map(|link| (format!("{link:?}"), throughput(cases, link)))
        .into_iter()
        .collect();
    println!("measured throughput, bytes/s: {links}");
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("results");
    fs::create_dir_all(&out)?;
    let table = json!({"throughput": links, "cases": rows, "regret": regret});
    fs::write(
        out.join("strategy_eval.json"),
        serde_json::to_string_pretty(&table)?,
    )?;
    Ok(())
}
