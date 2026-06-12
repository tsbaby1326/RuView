//! `AutomationEngine` — subscribes to the HOMECORE event bus, evaluates
//! triggers, and runs automation action sequences.
//!
//! ADR-129 §2 design: one Tokio task per running automation instance.
//!
//! ## Run modes (ADR-161, HC-WS-05)
//!
//! `RunMode::Single` is enforced via a per-automation `AtomicBool`
//! guard: while an instance is executing, a second trigger is skipped.
//! `Parallel` (and the as-yet-unbounded `Restart`/`Queued`) spawn a
//! fresh instance on every trigger. (Before this fix the doc claimed
//! AtomicBool enforcement but every trigger spawned unbounded parallel
//! tasks regardless of `mode`.)
//!
//! ## Time triggers (ADR-161, HC-WS-04)
//!
//! `Trigger::Time { at: "HH:MM:SS" }` is evaluated by a wall-clock timer
//! task (1 Hz tokio interval) — `Trigger::matches_sync` returns false for
//! `Time` because it has no clock. The timer fires each `time:`
//! automation once when the local wall-clock second equals its `at`.
//!
//! ## Template conditions (ADR-161, HC-WS-07)
//!
//! The engine builds a real [`TemplateEnvironment`] over the state
//! machine and passes it into every `EvalContext` (via
//! `EvalContext::with_templates`), so `template:` conditions evaluate
//! against live state instead of always returning false.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{Local, Timelike};
use tokio::sync::broadcast;

use homecore::HomeCore;

use crate::action::ExecutionContext;
use crate::automation::{Automation, RunMode};
use crate::condition::EvalContext;
use crate::template::TemplateEnvironment;
use crate::trigger::{Trigger, TriggerContext};

/// An automation registered with the engine, plus its runtime run-state.
struct Registered {
    auto: Arc<Automation>,
    /// `true` while a `Single`-mode instance is executing. Used to
    /// skip re-entrant triggers (HC-WS-05).
    running: Arc<AtomicBool>,
}

/// The automation engine. Holds a HOMECORE handle and a list of registered
/// automations. Call `start()` to begin listening for events.
pub struct AutomationEngine {
    hc: HomeCore,
    automations: Arc<Mutex<Vec<Registered>>>,
    templates: Arc<TemplateEnvironment>,
}

impl AutomationEngine {
    /// Create a new engine backed by the given HOMECORE handle.
    pub fn new(hc: HomeCore) -> Self {
        let templates = Arc::new(TemplateEnvironment::new(Arc::new(hc.states().clone())));
        Self {
            hc,
            automations: Arc::new(Mutex::new(vec![])),
            templates,
        }
    }

    /// Register an automation. Can be called before or after `start()`.
    pub fn register(&self, automation: Automation) {
        self.automations.lock().unwrap().push(Registered {
            auto: Arc::new(automation),
            running: Arc::new(AtomicBool::new(false)),
        });
    }

    /// Number of registered automations.
    pub fn len(&self) -> usize {
        self.automations.lock().unwrap().len()
    }

    /// Is the engine holding zero automations?
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Build an `EvalContext` with the engine's template environment
    /// wired in, over a fresh snapshot of the state machine.
    fn eval_ctx(&self) -> EvalContext {
        EvalContext::with_templates(
            Arc::new(self.hc.states().clone()),
            Arc::clone(&self.templates),
        )
    }

    /// Subscribe to the state-machine broadcast channel and start
    /// evaluating triggers. Also starts the wall-clock timer task that
    /// evaluates `time:` triggers. Returns a join handle for the event
    /// task (the timer task is detached and tied to the engine handle's
    /// lifetime via the broadcast channel close).
    ///
    /// The task runs until the broadcast sender is dropped (i.e. the
    /// `HomeCore` instance is destroyed).
    pub fn start(&self) -> tokio::task::JoinHandle<()> {
        self.start_timer();
        self.start_event_loop()
    }

    /// Event-driven loop: state/numeric/event triggers.
    fn start_event_loop(&self) -> tokio::task::JoinHandle<()> {
        let mut rx = self.hc.states().subscribe();
        let automations = Arc::clone(&self.automations);
        let hc = self.hc.clone();
        let templates = Arc::clone(&self.templates);

        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let snapshot: Vec<(Arc<Automation>, Arc<AtomicBool>)> = automations
                            .lock()
                            .unwrap()
                            .iter()
                            .map(|r| (Arc::clone(&r.auto), Arc::clone(&r.running)))
                            .collect();
                        for (automation, running) in snapshot {
                            if !automation.enabled {
                                continue;
                            }
                            let trigger_ctx = TriggerContext::state_changed(
                                event.entity_id.clone(),
                                event.old_state.clone(),
                                event.new_state.clone(),
                            );
                            let triggered = automation
                                .trigger
                                .iter()
                                .any(|t| t.matches_sync(&trigger_ctx));
                            if !triggered {
                                continue;
                            }
                            // Conditions (with template env wired in — HC-WS-07).
                            let eval_ctx = EvalContext::with_templates(
                                Arc::new(hc.states().clone()),
                                Arc::clone(&templates),
                            );
                            if !conditions_pass(&automation, &eval_ctx).await {
                                continue;
                            }
                            spawn_run(&hc, automation, running);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[homecore-automation] state-changed receiver lagged by {n} events");
                    }
                }
            }
        })
    }

    /// Wall-clock timer task: fires `time:` triggers (HC-WS-04). Ticks at
    /// 1 Hz and runs each matching automation once when the local
    /// wall-clock `HH:MM:SS` equals the trigger's `at`. The task exits
    /// when the state-machine broadcast channel closes (engine teardown).
    fn start_timer(&self) -> tokio::task::JoinHandle<()> {
        let automations = Arc::clone(&self.automations);
        let hc = self.hc.clone();
        let templates = Arc::clone(&self.templates);
        // A receiver that lets the timer notice engine teardown.
        let mut teardown_rx = self.hc.states().subscribe();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(1000));
            // Track the last second we fired, to fire once per match.
            let mut last_fired_sec: Option<String> = None;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let now = Local::now();
                        let hhmmss = format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second());
                        if last_fired_sec.as_deref() == Some(hhmmss.as_str()) {
                            continue;
                        }
                        let snapshot: Vec<(Arc<Automation>, Arc<AtomicBool>)> = automations
                            .lock()
                            .unwrap()
                            .iter()
                            .map(|r| (Arc::clone(&r.auto), Arc::clone(&r.running)))
                            .collect();
                        let mut fired_any = false;
                        for (automation, running) in snapshot {
                            if !automation.enabled {
                                continue;
                            }
                            let time_match = automation.trigger.iter().any(|t| match t {
                                Trigger::Time { at } => time_at_matches(at, &hhmmss),
                                _ => false,
                            });
                            if !time_match {
                                continue;
                            }
                            let eval_ctx = EvalContext::with_templates(
                                Arc::new(hc.states().clone()),
                                Arc::clone(&templates),
                            );
                            if !conditions_pass(&automation, &eval_ctx).await {
                                continue;
                            }
                            spawn_run(&hc, automation, running);
                            fired_any = true;
                        }
                        if fired_any {
                            last_fired_sec = Some(hhmmss);
                        }
                    }
                    r = teardown_rx.recv() => {
                        if let Err(broadcast::error::RecvError::Closed) = r {
                            break;
                        }
                    }
                }
            }
        })
    }

    /// Manually fire any `time:` automations whose `at` equals `hhmmss`
    /// (`"HH:MM:SS"`). Bypasses the 1 Hz clock so tests can assert the
    /// time-trigger path deterministically without waiting for a
    /// wall-clock second to roll over. Returns the number of automations
    /// that fired (passed conditions and were spawned).
    pub async fn fire_time_for_test(&self, hhmmss: &str) -> usize {
        let snapshot: Vec<(Arc<Automation>, Arc<AtomicBool>)> = self
            .automations
            .lock()
            .unwrap()
            .iter()
            .map(|r| (Arc::clone(&r.auto), Arc::clone(&r.running)))
            .collect();
        let mut fired = 0usize;
        for (automation, running) in snapshot {
            if !automation.enabled {
                continue;
            }
            let time_match = automation.trigger.iter().any(|t| match t {
                Trigger::Time { at } => time_at_matches(at, hhmmss),
                _ => false,
            });
            if !time_match {
                continue;
            }
            let eval_ctx = self.eval_ctx();
            if !conditions_pass(&automation, &eval_ctx).await {
                continue;
            }
            spawn_run(&self.hc, automation, running);
            fired += 1;
        }
        fired
    }
}

/// Evaluate all of an automation's conditions (AND). Empty → pass.
async fn conditions_pass(automation: &Automation, eval_ctx: &EvalContext) -> bool {
    for cond in &automation.condition {
        if !cond.evaluate(eval_ctx).await {
            return false;
        }
    }
    true
}

/// Does a `Time` trigger `at` value match the current `HH:MM:SS`?
/// Accepts `HH:MM` (matches at :00 seconds) and `HH:MM:SS`.
fn time_at_matches(at: &str, hhmmss: &str) -> bool {
    let normalized = match at.matches(':').count() {
        1 => format!("{at}:00"),
        _ => at.to_string(),
    };
    normalized == hhmmss
}

/// Spawn an automation run, honoring `RunMode::Single` re-entrancy
/// guard (HC-WS-05). For `Single`/`IgnoreFirst` modes a run already in
/// flight causes the new trigger to be skipped; the `running` flag is
/// cleared when the run finishes.
fn spawn_run(hc: &HomeCore, automation: Arc<Automation>, running: Arc<AtomicBool>) {
    let single = matches!(automation.mode, RunMode::Single | RunMode::IgnoreFirst);
    if single {
        // Try to claim the running slot; if already running, skip.
        if running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
    }
    let hc_clone = hc.clone();
    tokio::spawn(async move {
        let mut exec_ctx = ExecutionContext::new(hc_clone, automation.id.clone());
        for action in &automation.action {
            if let Err(e) = action.execute(&mut exec_ctx).await {
                eprintln!("[homecore-automation] action error in {}: {e}", automation.id);
                break;
            }
        }
        if single {
            running.store(false, Ordering::SeqCst);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::automation::Automation;
    use crate::trigger::Trigger;
    use homecore::{Context, EntityId, HomeCore, ServiceCall, ServiceName};
    use homecore::service::FnHandler;
    use std::sync::{Arc, Mutex};
    use tokio::time::{sleep, Duration};

    /// Register a recording handler that captures all calls.
    async fn register_recorder(
        hc: &HomeCore,
        domain: &str,
        service: &str,
    ) -> Arc<Mutex<Vec<serde_json::Value>>> {
        let log: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(vec![]));
        let log2 = Arc::clone(&log);
        hc.services()
            .register(
                ServiceName::new(domain, service),
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

    #[tokio::test]
    async fn engine_fires_automation_on_state_change() {
        let hc = HomeCore::new();
        let log = register_recorder(&hc, "light", "turn_on").await;

        let engine = AutomationEngine::new(hc.clone());
        engine.register(Automation::new(
            "test_auto_1",
            vec![Trigger::State {
                entity_id: EntityId::parse("switch.living").unwrap(),
                from: None,
                to: Some("on".into()),
            }],
            vec![Action::ServiceCall {
                domain: "light".into(),
                service: "turn_on".into(),
                data: serde_json::json!({"brightness": 100}),
            }],
        ));

        let _handle = engine.start();

        hc.states().set(
            EntityId::parse("switch.living").unwrap(),
            "on",
            serde_json::json!({}),
            Context::new(),
        );

        sleep(Duration::from_millis(50)).await;

        assert_eq!(log.lock().unwrap().len(), 1);
        assert_eq!(log.lock().unwrap()[0]["brightness"], 100);
    }

    #[tokio::test]
    async fn engine_does_not_fire_on_wrong_entity() {
        let hc = HomeCore::new();
        let log = register_recorder(&hc, "light", "turn_on").await;

        let engine = AutomationEngine::new(hc.clone());
        engine.register(Automation::new(
            "test_auto_2",
            vec![Trigger::State {
                entity_id: EntityId::parse("switch.living").unwrap(),
                from: None,
                to: Some("on".into()),
            }],
            vec![Action::ServiceCall {
                domain: "light".into(),
                service: "turn_on".into(),
                data: serde_json::json!({}),
            }],
        ));

        let _handle = engine.start();

        hc.states().set(
            EntityId::parse("switch.bedroom").unwrap(),
            "on",
            serde_json::json!({}),
            Context::new(),
        );

        sleep(Duration::from_millis(50)).await;
        assert_eq!(log.lock().unwrap().len(), 0, "should not fire on wrong entity");
    }

    #[tokio::test]
    async fn engine_disabled_automation_does_not_fire() {
        let hc = HomeCore::new();
        let log = register_recorder(&hc, "light", "turn_on").await;

        let engine = AutomationEngine::new(hc.clone());
        let mut auto = Automation::new(
            "test_auto_3",
            vec![Trigger::State {
                entity_id: EntityId::parse("switch.living").unwrap(),
                from: None,
                to: Some("on".into()),
            }],
            vec![Action::ServiceCall {
                domain: "light".into(),
                service: "turn_on".into(),
                data: serde_json::json!({}),
            }],
        );
        auto.enabled = false;
        engine.register(auto);

        let _handle = engine.start();

        hc.states().set(
            EntityId::parse("switch.living").unwrap(),
            "on",
            serde_json::json!({}),
            Context::new(),
        );

        sleep(Duration::from_millis(50)).await;
        assert_eq!(log.lock().unwrap().len(), 0, "disabled automation should not fire");
    }

    // Behavioral tests for the timer / run-mode / template paths
    // (HC-WS-04/05/07) live in `tests/engine_behaviors.rs` to keep this
    // file under the 500-line guideline; they use only the public API.

    #[test]
    fn time_at_matches_handles_hh_mm_and_hh_mm_ss() {
        assert!(time_at_matches("07:30", "07:30:00"));
        assert!(time_at_matches("07:30:15", "07:30:15"));
        assert!(!time_at_matches("07:30", "07:30:01"));
        assert!(!time_at_matches("07:30:15", "07:30:16"));
    }
}
