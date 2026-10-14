//! Jev against the fixed deciders and the rule, scored by regret
//! against the measured best strategy. Jev is asked about each case with the client's own
//! question. Reads the AI Gateway key from `~/.config/herdfs/ai-gateway-key`, and writes the
//! table to `results/strategy_eval.json`.
//! contract: P2, P3

mod cases;
mod workload;

use std::path::Path;
use std::time::Instant;
use std::{env, fs};

use anyhow::{Context, Result, ensure};
use herdfs_client::{AccessPlan, Jev, JevAnswer, OpenContext};
use serde_json::json;

use crate::cases::{Case, MIB, PLANS, measure_all};

const JEV_URL: &str = "https://ai-gateway.vercel.sh/v1/evaluate";

// contract: P3
#[test]
fn fixed_deciders_cover_every_plan_setting() {
    let mut names: Vec<String> = PLANS.iter().map(|(_, plan, _)| plan.name()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 24, "every finite plan is measured once");
    assert!(PLANS.iter().any(|(name, ..)| *name == "stream"));
    assert!(PLANS.iter().any(|(name, ..)| *name == "fetch"));
    assert!(PLANS.iter().any(|(name, ..)| *name == "pin"));
    assert!(PLANS.iter().any(|(_, plan, _)| plan.name() == "index/none/no"));
    assert!(PLANS.iter().any(|(_, plan, _)| plan.name() == "none/large/no"));
}

/// Jev's raw answer for a case (`None` if the ask failed), and how long it took.
struct Asked {
    answer: Option<JevAnswer>,
    millis: u128,
}

#[test]
#[ignore = "R5 asks real Jev over the network"]
fn jev_beats_every_fixed_strategy_by_regret() -> Result<()> {
    let key_file = Path::new(&env::var("HOME")?).join(".config/herdfs/ai-gateway-key");
    let jev = Jev::new(&key_file, JEV_URL).with_context(|| format!("reading {}", key_file.display()))?;
    let cases = measure_all()?;
    let contexts: Vec<OpenContext> = cases.iter().map(context).collect();
    let asked = ask_all(&jev, &contexts)?;
    // As the client's decider does: an unsure or failed answer leaves the rule's choice.
    let by_jev = contexts.iter().zip(&asked).map(|(context, asked)| {
        asked
            .answer
            .map_or_else(|| rule(context), |answer| answer.plan(rule(context)))
    });
    let mut deciders: Vec<(&str, Vec<AccessPlan>)> = PLANS
        .into_iter()
        .map(|(name, plan, _)| (name, vec![plan; cases.len()]))
        .collect();
    deciders.push(("rule", contexts.iter().map(rule).collect()));
    deciders.push(("jev", by_jev.collect()));
    let scores: Vec<(f64, f64)> = deciders
        .iter()
        .map(|(_, choices)| score(&cases, choices))
        .collect();
    report(&cases, &asked, &deciders, &scores)?;

    let best_fixed = scores[..PLANS.len()]
        .iter()
        .map(|(mean, _)| *mean)
        .fold(f64::INFINITY, f64::min);
    let (mean, p90) = scores[PLANS.len() + 1];
    ensure!(
        mean < best_fixed && p90 <= 1.5,
        "Jev's regret: mean {mean:.2} (the best fixed decider's is {best_fixed:.2}), p90 {p90:.2} (at most 1.5)"
    );
    Ok(())
}

/// The context a fresh client has at the case's first open: no download measured yet, and the
/// round trip its first calls to the authority measured.
fn context(case: &Case) -> OpenContext {
    OpenContext {
        size: case.mib * MIB,
        extension: Some("bin".into()),
        comm: case.workload.comm().into(),
        coverage: 0.0,
        throughput: None,
        round_trip: case.round_trip,
        opens: 0,
    }
}

/// The client's rule decider: fetch files under 64 MiB whole, stream larger ones.
fn rule(context: &OpenContext) -> AccessPlan {
    if context.size < 64 * MIB {
        AccessPlan::FETCH
    } else {
        AccessPlan::STREAM
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

/// Mean and p90 regret of `choices`, one per case.
fn score(cases: &[Case], choices: &[AccessPlan]) -> (f64, f64) {
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
    deciders: &[(&str, Vec<AccessPlan>)],
    scores: &[(f64, f64)],
) -> Result<()> {
    println!(
        "{:<22} {:<16} {:<16} {:<16} jev's answer",
        "case", "best", "rule", "jev"
    );
    let mut rows = Vec::new();
    for (index, (case, asked)) in cases.iter().zip(asked).enumerate() {
        let decisions: Vec<String> = deciders
            .iter()
            .map(|(_, choices)| choices[index].name())
            .collect();
        let answer = asked.answer;
        let (best, name) = (case.best().name(), case.name());
        let said = match answer {
            Some(answer) => serde_json::to_string(&answer)?,
            None => "failed".into(),
        };
        println!(
            "{name:<22} {best:<16} {:<16} {:<16} {said} in {} ms",
            decisions[PLANS.len()],
            decisions[PLANS.len() + 1],
            asked.millis
        );
        rows.push(json!({
            "case": name, "seconds": &case.seconds, "bytes": &case.bytes, "best": best, "round_trip": case.round_trip,
            "rule": decisions[PLANS.len()], "jev": decisions[PLANS.len() + 1], "jev_answer": answer, "jev_ms": asked.millis,
        }));
    }
    let mut regret = serde_json::Map::new();
    for ((name, _), (mean, p90)) in deciders.iter().zip(scores) {
        println!("{name:<7} regret: mean {mean:.2}, p90 {p90:.2}");
        regret.insert((*name).into(), json!({"mean": mean, "p90": p90}));
    }
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("results");
    fs::create_dir_all(&out)?;
    let table = json!({"cases": rows, "regret": regret});
    fs::write(
        out.join("strategy_eval.json"),
        serde_json::to_string_pretty(&table)?,
    )?;
    Ok(())
}
