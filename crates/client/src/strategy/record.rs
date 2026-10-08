use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use serde::Serialize;

use super::{AccessPlan, Confidence, PlanSource};
use crate::lock;

#[derive(Clone, Copy, Serialize)]
struct Decision {
    plan: AccessPlan,
    source: PlanSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<Confidence>,
}

pub(crate) struct AccessRecord {
    decision: Mutex<Decision>,
    pub bytes_fetched: AtomicU64,
}

impl AccessRecord {
    pub fn new(plan: AccessPlan, source: PlanSource, confidence: Option<Confidence>) -> Self {
        Self {
            decision: Mutex::new(Decision {
                plan,
                source,
                confidence,
            }),
            bytes_fetched: AtomicU64::new(0),
        }
    }

    pub fn plan(&self) -> AccessPlan {
        lock(&self.decision).plan
    }

    pub fn update(&self, plan: AccessPlan, source: PlanSource, confidence: Option<Confidence>) {
        *lock(&self.decision) = Decision {
            plan,
            source,
            confidence,
        };
    }

    pub fn json(&self) -> Vec<u8> {
        let decision = *lock(&self.decision);
        let mut value = serde_json::json!({
            "plan": decision.plan,
            "source": decision.source,
            "bytes_fetched": self.bytes_fetched.load(Relaxed),
        });
        if let Some(confidence) = decision.confidence {
            value["confidence"] = serde_json::json!(confidence);
        }
        value.to_string().into_bytes()
    }
}
