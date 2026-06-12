//! `Action` enum and async execution.
//!
//! Implements the ADR-129 P1 action set: `service_call`, `delay`, `scene`,
//! `wait_for_trigger`, `choose`. Complex variants (parallel, repeat, if,
//! stop, fire_event, wait_template) land in P2.
//!
//! ## `choose` branch evaluation (ADR-161, HC-WS-06)
//!
//! `Action::Choose` evaluates each branch's `conditions` against the live
//! [`EvalContext`] (deserialising the per-branch `serde_yaml::Value`
//! conditions into [`Condition`]) and runs the FIRST matching branch's
//! sequence. Only if no branch matches does it fall to `default`. Before
//! this fix the branches were discarded and `default` always ran.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use homecore::{Context, HomeCore, ServiceCall, ServiceName, StateMachine};

use crate::condition::{Condition, EvalContext};
use crate::error::AutomationError;
use crate::template::TemplateEnvironment;

/// Runtime context passed into action execution.
pub struct ExecutionContext {
    /// HOMECORE handle — provides service registry + state machine.
    pub hc: HomeCore,
    /// Causality context for service calls triggered by this automation.
    pub context: Context,
    /// Automation ID for tracing/logging.
    pub automation_id: String,
    /// Condition-evaluation context for `Choose` branches. Carries the
    /// state-machine snapshot + optional template environment so branch
    /// conditions (incl. `template:`) evaluate against live state.
    pub eval: EvalContext,
}

impl ExecutionContext {
    /// Build a context whose `Choose` branches evaluate against the
    /// HomeCore state machine (no template env — `template:` branch
    /// conditions evaluate false; use [`Self::with_templates`] to wire
    /// one).
    pub fn new(hc: HomeCore, automation_id: impl Into<String>) -> Self {
        let sm = Arc::new(hc.states().clone());
        Self {
            hc,
            context: Context::new(),
            automation_id: automation_id.into(),
            eval: EvalContext::new(sm),
        }
    }

    /// Build a context with a template environment wired into the
    /// `Choose` branch-condition evaluator.
    pub fn with_templates(
        hc: HomeCore,
        automation_id: impl Into<String>,
        states: Arc<StateMachine>,
        templates: Arc<TemplateEnvironment>,
    ) -> Self {
        Self {
            hc,
            context: Context::new(),
            automation_id: automation_id.into(),
            eval: EvalContext::with_templates(states, templates),
        }
    }
}

/// Action configuration. Deserialized from YAML `action:` blocks.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Call a HOMECORE service.
    ServiceCall {
        domain: String,
        service: String,
        #[serde(default)]
        data: serde_json::Value,
    },
    /// Pause execution for a fixed duration (ISO 8601 or seconds float).
    Delay {
        /// Delay in seconds.
        seconds: f64,
    },
    /// Activate a named scene entity.
    Scene {
        scene: String,
    },
    /// Block until one of the listed triggers fires (or timeout).
    WaitForTrigger {
        timeout_seconds: Option<f64>,
    },
    /// Conditional branching — first matching branch wins.
    Choose {
        choices: Vec<ChoiceBranch>,
        #[serde(default)]
        default: Vec<Action>,
    },
}

/// A single branch in a `Choose` action.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChoiceBranch {
    pub conditions: Vec<serde_yaml::Value>,
    pub sequence: Vec<Action>,
}

impl ChoiceBranch {
    /// Does this branch match? All of its `conditions` must evaluate
    /// true (HA `choose` semantics are AND-over-conditions). Each raw
    /// `serde_yaml::Value` is deserialised into a [`Condition`]; a
    /// condition that fails to parse is treated as non-matching (the
    /// branch is skipped) rather than silently passing. An empty
    /// `conditions` list matches (an unconditional branch).
    pub async fn matches(&self, eval: &EvalContext) -> bool {
        for raw in &self.conditions {
            let cond: Condition = match serde_yaml::from_value(raw.clone()) {
                Ok(c) => c,
                Err(_) => return false,
            };
            if !cond.evaluate(eval).await {
                return false;
            }
        }
        true
    }
}

impl Action {
    /// Execute this action using the provided context.
    ///
    /// Returns a JSON value (may be `null`) for callers that chain
    /// `wait_for_trigger` / `set_variable` patterns (P2).
    ///
    /// Uses `Box::pin` for recursive variants (Choose) to satisfy the
    /// Rust requirement that recursive async fns introduce indirection.
    pub fn execute<'a>(
        &'a self,
        ctx: &'a mut ExecutionContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value, AutomationError>> + Send + 'a>> {
        Box::pin(async move {
            match self {
                Action::ServiceCall { domain, service, data } => {
                    let call = ServiceCall {
                        name: ServiceName::new(domain.clone(), service.clone()),
                        data: data.clone(),
                        context: ctx.context.clone(),
                    };
                    let result = ctx.hc.services().call(call).await?;
                    Ok(result)
                }
                Action::Delay { seconds } => {
                    let dur = Duration::from_secs_f64(*seconds);
                    sleep(dur).await;
                    Ok(serde_json::Value::Null)
                }
                Action::Scene { scene } => {
                    // Scene activation maps to homeassistant.turn_on with entity_id = scene
                    let call = ServiceCall {
                        name: ServiceName::new("homeassistant", "turn_on"),
                        data: serde_json::json!({ "entity_id": scene }),
                        context: ctx.context.clone(),
                    };
                    let result = ctx.hc.services().call(call).await?;
                    Ok(result)
                }
                Action::WaitForTrigger { timeout_seconds } => {
                    // P1 stub — just sleeps for the timeout duration if specified.
                    // Full trigger subscription lands in P2.
                    if let Some(secs) = timeout_seconds {
                        sleep(Duration::from_secs_f64(*secs)).await;
                    }
                    Ok(serde_json::Value::Null)
                }
                Action::Choose { choices, default } => {
                    // Evaluate each branch's conditions against live state;
                    // run the first branch whose conditions ALL pass. Fall
                    // to `default` only if no branch matches (HC-WS-06).
                    for branch in choices {
                        if branch.matches(&ctx.eval).await {
                            for a in &branch.sequence {
                                a.execute(ctx).await?;
                            }
                            return Ok(serde_json::Value::Null);
                        }
                    }
                    for a in default {
                        a.execute(ctx).await?;
                    }
                    Ok(serde_json::Value::Null)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use homecore::{HomeCore, ServiceCall, ServiceError, ServiceName};
    use homecore::service::FnHandler;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn service_call_action_fires_handler() {
        let hc = HomeCore::new();
        let log: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(vec![]));
        let log2 = Arc::clone(&log);
        hc.services()
            .register(
                ServiceName::new("light", "turn_on"),
                FnHandler(move |call: ServiceCall| {
                    let log3 = Arc::clone(&log2);
                    async move {
                        log3.lock().unwrap().push(call.data.clone());
                        Ok(call.data)
                    }
                }),
            )
            .await;

        let action = Action::ServiceCall {
            domain: "light".into(),
            service: "turn_on".into(),
            data: serde_json::json!({"brightness": 255}),
        };
        let mut exec_ctx = ExecutionContext::new(hc, "test_auto");
        let res = action.execute(&mut exec_ctx).await.unwrap();
        assert_eq!(res["brightness"], 255);
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delay_action_completes() {
        let hc = HomeCore::new();
        let mut exec_ctx = ExecutionContext::new(hc, "test_auto");
        let action = Action::Delay { seconds: 0.001 };
        let result = action.execute(&mut exec_ctx).await.unwrap();
        assert!(result.is_null());
    }

    #[tokio::test]
    async fn service_call_unregistered_returns_error() {
        let hc = HomeCore::new();
        let mut exec_ctx = ExecutionContext::new(hc, "test_auto");
        let action = Action::ServiceCall {
            domain: "light".into(),
            service: "turn_on".into(),
            data: serde_json::json!({}),
        };
        let err = action.execute(&mut exec_ctx).await.unwrap_err();
        assert!(matches!(err, AutomationError::ServiceCall(ServiceError::NotRegistered { .. })));
    }

    /// Register two recording handlers and return their call logs.
    async fn two_recorders(
        hc: &HomeCore,
    ) -> (Arc<Mutex<Vec<serde_json::Value>>>, Arc<Mutex<Vec<serde_json::Value>>>) {
        use homecore::EntityId;
        let _ = EntityId::parse("light.x"); // touch import path
        let mk = |hc: &HomeCore, svc: &'static str| {
            let log: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(vec![]));
            let log2 = Arc::clone(&log);
            let hc = hc.clone();
            async move {
                hc.services()
                    .register(
                        ServiceName::new("light", svc),
                        FnHandler(move |call: ServiceCall| {
                            let l = Arc::clone(&log2);
                            async move {
                                l.lock().unwrap().push(call.data.clone());
                                Ok(serde_json::Value::Null)
                            }
                        }),
                    )
                    .await;
                log
            }
        };
        let branch_log = mk(hc, "branch_service").await;
        let default_log = mk(hc, "default_service").await;
        (branch_log, default_log)
    }

    fn choose_with_match() -> Action {
        // A `Choose` whose first branch requires light.gate == "open".
        let branch_conditions = vec![serde_yaml::from_str::<serde_yaml::Value>(
            "condition: state\nentity_id: light.gate\nstate: open",
        )
        .unwrap()];
        Action::Choose {
            choices: vec![ChoiceBranch {
                conditions: branch_conditions,
                sequence: vec![Action::ServiceCall {
                    domain: "light".into(),
                    service: "branch_service".into(),
                    data: serde_json::json!({"branch": true}),
                }],
            }],
            default: vec![Action::ServiceCall {
                domain: "light".into(),
                service: "default_service".into(),
                data: serde_json::json!({"default": true}),
            }],
        }
    }

    #[tokio::test]
    async fn choose_runs_matching_branch_not_default() {
        // HC-WS-06: with the branch condition satisfied, the branch
        // sequence runs and `default` does NOT. On the pre-fix code
        // (choices discarded) `default` ran instead → this fails on old.
        use homecore::{Context, EntityId};
        let hc = HomeCore::new();
        let (branch_log, default_log) = two_recorders(&hc).await;
        hc.states().set(
            EntityId::parse("light.gate").unwrap(),
            "open",
            serde_json::json!({}),
            Context::new(),
        );

        let mut ctx = ExecutionContext::new(hc, "choose_auto");
        choose_with_match().execute(&mut ctx).await.unwrap();

        assert_eq!(branch_log.lock().unwrap().len(), 1, "matching branch must run");
        assert_eq!(default_log.lock().unwrap().len(), 0, "default must NOT run when a branch matches");
    }

    #[tokio::test]
    async fn choose_falls_to_default_when_no_branch_matches() {
        use homecore::{Context, EntityId};
        let hc = HomeCore::new();
        let (branch_log, default_log) = two_recorders(&hc).await;
        // gate is "closed" → branch condition (== "open") fails.
        hc.states().set(
            EntityId::parse("light.gate").unwrap(),
            "closed",
            serde_json::json!({}),
            Context::new(),
        );

        let mut ctx = ExecutionContext::new(hc, "choose_auto");
        choose_with_match().execute(&mut ctx).await.unwrap();

        assert_eq!(branch_log.lock().unwrap().len(), 0, "branch must not run when condition fails");
        assert_eq!(default_log.lock().unwrap().len(), 1, "default must run when no branch matches");
    }
}
