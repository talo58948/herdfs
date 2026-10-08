use std::str::FromStr;

use rand::{SeedableRng, rngs::SmallRng};

use super::{AccessPlan, Decider, Keep, Prefetch, ReadAhead, Setting};

/// A `--strategy` value. Jev needs the key from `--jev-key-file` before it can decide.
#[derive(Clone)]
pub(crate) enum StrategyArg {
    Decider(Decider),
    Jev,
}

impl FromStr for StrategyArg {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        let unknown = || {
            format!(
                "unknown strategy {value:?}; use stream, fetch, pin, rule, chaos:SEED, jev or PREFETCH/READ_AHEAD/KEEP"
            )
        };
        let decider = match value {
            "jev" => return Ok(Self::Jev),
            "rule" => Decider::Rule,
            "stream" => Decider::Fixed(AccessPlan::STREAM),
            "fetch" => Decider::Fixed(AccessPlan::FETCH),
            "pin" => Decider::Fixed(AccessPlan::PIN),
            _ => match value.strip_prefix("chaos:") {
                Some(seed) => {
                    let seed = seed.parse().map_err(|_| format!("invalid chaos seed {seed:?}"))?;
                    Decider::Chaos(SmallRng::seed_from_u64(seed))
                }
                None => Decider::Fixed(parse_plan(value).map_err(|_| unknown())?),
            },
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
    let prefetch = Prefetch::parse(prefetch).ok_or_else(|| format!("unknown prefetch {prefetch:?}"))?;
    let read_ahead =
        ReadAhead::parse(read_ahead).ok_or_else(|| format!("unknown read-ahead {read_ahead:?}"))?;
    let keep = Keep::parse(keep).ok_or_else(|| format!("unknown keep value {keep:?}"))?;
    Ok(AccessPlan::new(prefetch, read_ahead, keep))
}
