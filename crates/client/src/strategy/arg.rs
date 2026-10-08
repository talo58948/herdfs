use std::str::FromStr;

use rand::{SeedableRng, rngs::SmallRng};

use super::{AccessPlan, Decider, Keep, Prefetch, ReadAhead};

/// A `--strategy` value. Jev needs the key from `--jev-key-file` before it can decide.
#[derive(Clone)]
pub enum StrategyArg {
    Decider(Decider),
    Jev,
}

impl FromStr for StrategyArg {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        let named = match value {
            "stream" => Some(AccessPlan::STREAM),
            "fetch" => Some(AccessPlan::FETCH),
            "pin" => Some(AccessPlan::PIN),
            _ => None,
        };
        let decider = match (value, named) {
            ("jev", _) => return Ok(Self::Jev),
            ("rule", _) => Decider::Rule,
            (_, Some(plan)) => Decider::Fixed(plan),
            _ if value.starts_with("chaos:") => {
                let seed = value.trim_start_matches("chaos:");
                let seed = seed.parse().map_err(|_| format!("invalid chaos seed {seed:?}"))?;
                Decider::Chaos(SmallRng::seed_from_u64(seed))
            }
            _ => Decider::Fixed(parse_plan(value)?),
        };
        Ok(Self::Decider(decider))
    }
}

fn parse_plan(value: &str) -> Result<AccessPlan, String> {
    let mut parts = value.split('/');
    let (Some(prefetch), Some(read_ahead), Some(keep), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(format!("unknown strategy {value:?}"));
    };
    let prefetch = Prefetch::ALL
        .into_iter()
        .find(|setting| setting.name() == prefetch)
        .ok_or_else(|| format!("unknown prefetch {prefetch:?}"))?;
    let read_ahead = ReadAhead::ALL
        .into_iter()
        .find(|setting| setting.name() == read_ahead)
        .ok_or_else(|| format!("unknown read-ahead {read_ahead:?}"))?;
    let keep = Keep::ALL
        .into_iter()
        .find(|setting| setting.name() == keep)
        .ok_or_else(|| format!("unknown keep value {keep:?}"))?;
    Ok(AccessPlan::new(prefetch, read_ahead, keep))
}
