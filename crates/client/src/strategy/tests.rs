use super::*;

fn context(size: u64) -> OpenContext {
    OpenContext {
        size,
        extension: None,
        comm: "cat".into(),
        coverage: 0.0,
        throughput: None,
        round_trip: 0.001,
        opens: 0,
    }
}

fn decider(value: &str) -> Decider {
    let Ok(StrategyArg::Decider(decider)) = value.parse() else {
        panic!("{value} names no local decider");
    };
    decider
}

// contract: P1, P3
#[test]
fn named_strategies_are_fixed_plans_and_rule_keeps_its_cutoff() {
    assert_eq!(decider("stream").decide(&context(1)).plan, AccessPlan::STREAM);
    assert_eq!(decider("fetch").decide(&context(1)).plan, AccessPlan::FETCH);
    assert_eq!(decider("pin").decide(&context(1)).plan, AccessPlan::PIN);
    assert_eq!(
        decider("rule").decide(&context(FETCH_BELOW - 1)).plan,
        AccessPlan::FETCH
    );
    assert_eq!(
        decider("rule").decide(&context(FETCH_BELOW)).plan,
        AccessPlan::STREAM
    );
}

// contract: P3, C8
#[test]
fn explicit_and_chaos_plans_choose_settings_independently() {
    let plan = decider("index/large/no").decide(&context(1)).plan;
    assert_eq!(plan, AccessPlan::new(Prefetch::Index, ReadAhead::Large, Keep::No));
    let picks = |seed: &str| {
        let mut chaos = decider(seed);
        (0..128)
            .map(|_| chaos.decide(&context(1)).plan)
            .collect::<Vec<_>>()
    };
    let first = picks("chaos:7");
    assert_eq!(first, picks("chaos:7"));
    assert_ne!(first, picks("chaos:8"));
    assert!(
        Prefetch::ALL
            .iter()
            .all(|value| first.iter().any(|plan| plan.prefetch == *value))
    );
    assert!(
        ReadAhead::ALL
            .iter()
            .all(|value| first.iter().any(|plan| plan.read_ahead == *value))
    );
    assert!(
        Keep::ALL
            .iter()
            .all(|value| first.iter().any(|plan| plan.keep == *value))
    );
}

#[test]
fn names_jev_and_rejects_malformed_plans() {
    assert!(matches!("jev".parse(), Ok(StrategyArg::Jev)));
    assert!("chaos:x".parse::<StrategyArg>().is_err());
    assert!("index/huge/no".parse::<StrategyArg>().is_err());
}
