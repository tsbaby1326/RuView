//! Engine behavioral integration tests (ADR-161, HC-WS-04/05/07).
//!
//! These exercise the `AutomationEngine` runtime through its public API
//! only (extracted from the inline module to keep `engine.rs` under the
//! 500-line file guideline):
//!
//! - HC-WS-04 — `time:` triggers fire via the engine timer path.
//! - HC-WS-05 — `RunMode::Single` does not double-fire; `Parallel` does.
//! - HC-WS-07 — `template:` conditions evaluate against live state in the
//!   engine path (no longer always-false).
//!
//! Each fails on the pre-fix engine (no timer task, unbounded-parallel
//! regardless of mode, `template_env: None`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use homecore::service::FnHandler;
use homecore::{Context, EntityId, HomeCore, ServiceCall, ServiceName};
use homecore_automation::{Action, Automation, AutomationEngine, Condition, RunMode, Trigger};
use tokio::time::{sleep, Duration};

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

// ── HC-WS-04: time triggers fire ───────────────────────────────────
#[tokio::test]
async fn time_trigger_fires_via_timer_path() {
    let hc = HomeCore::new();
    let log = register_recorder(&hc, "light", "turn_on").await;

    let engine = AutomationEngine::new(hc.clone());
    engine.register(Automation::new(
        "time_auto",
        vec![Trigger::Time { at: "07:30:00".into() }],
        vec![Action::ServiceCall {
            domain: "light".into(),
            service: "turn_on".into(),
            data: serde_json::json!({"by": "time"}),
        }],
    ));

    // Deterministically fire the timer path for the matching second.
    let fired = engine.fire_time_for_test("07:30:00").await;
    assert_eq!(fired, 1, "time automation should fire for matching HH:MM:SS");
    sleep(Duration::from_millis(50)).await;
    assert_eq!(log.lock().unwrap().len(), 1, "time trigger should run its action");

    // A non-matching second must NOT fire.
    let none = engine.fire_time_for_test("09:00:00").await;
    assert_eq!(none, 0);
}

// ── HC-WS-05: RunMode::Single does not double-fire ─────────────────
#[tokio::test]
async fn single_mode_does_not_double_fire_on_rapid_triggers() {
    let hc = HomeCore::new();
    let count = Arc::new(AtomicUsize::new(0));
    let count2 = Arc::clone(&count);
    hc.services()
        .register(
            ServiceName::new("light", "slow"),
            FnHandler(move |_call: ServiceCall| {
                let c = Arc::clone(&count2);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    sleep(Duration::from_millis(200)).await;
                    Ok(serde_json::Value::Null)
                }
            }),
        )
        .await;

    let engine = AutomationEngine::new(hc.clone());
    let mut auto = Automation::new(
        "single_auto",
        vec![Trigger::State {
            entity_id: EntityId::parse("switch.s").unwrap(),
            from: None,
            to: None,
        }],
        vec![Action::ServiceCall {
            domain: "light".into(),
            service: "slow".into(),
            data: serde_json::json!({}),
        }],
    );
    auto.mode = RunMode::Single;
    engine.register(auto);
    let _handle = engine.start();

    // Two rapid triggers while the first run is still sleeping.
    hc.states().set(EntityId::parse("switch.s").unwrap(), "a", serde_json::json!({}), Context::new());
    sleep(Duration::from_millis(20)).await;
    hc.states().set(EntityId::parse("switch.s").unwrap(), "b", serde_json::json!({}), Context::new());

    sleep(Duration::from_millis(350)).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "Single-mode automation must not double-fire while already running"
    );
}

#[tokio::test]
async fn parallel_mode_does_fire_concurrently() {
    let hc = HomeCore::new();
    let count = Arc::new(AtomicUsize::new(0));
    let count2 = Arc::clone(&count);
    hc.services()
        .register(
            ServiceName::new("light", "slow"),
            FnHandler(move |_call: ServiceCall| {
                let c = Arc::clone(&count2);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    sleep(Duration::from_millis(150)).await;
                    Ok(serde_json::Value::Null)
                }
            }),
        )
        .await;

    let engine = AutomationEngine::new(hc.clone());
    let mut auto = Automation::new(
        "parallel_auto",
        vec![Trigger::State {
            entity_id: EntityId::parse("switch.p").unwrap(),
            from: None,
            to: None,
        }],
        vec![Action::ServiceCall {
            domain: "light".into(),
            service: "slow".into(),
            data: serde_json::json!({}),
        }],
    );
    auto.mode = RunMode::Parallel;
    engine.register(auto);
    let _handle = engine.start();

    hc.states().set(EntityId::parse("switch.p").unwrap(), "a", serde_json::json!({}), Context::new());
    sleep(Duration::from_millis(20)).await;
    hc.states().set(EntityId::parse("switch.p").unwrap(), "b", serde_json::json!({}), Context::new());

    sleep(Duration::from_millis(300)).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "Parallel-mode automation should fire on every trigger"
    );
}

// ── HC-WS-07: template conditions evaluate in the engine path ──────
#[tokio::test]
async fn template_condition_evaluates_true_in_engine() {
    let hc = HomeCore::new();
    let log = register_recorder(&hc, "light", "turn_on").await;

    hc.states().set(
        EntityId::parse("sensor.flag").unwrap(),
        "on",
        serde_json::json!({}),
        Context::new(),
    );

    let engine = AutomationEngine::new(hc.clone());
    let mut auto = Automation::new(
        "tmpl_auto",
        vec![Trigger::State {
            entity_id: EntityId::parse("switch.trigger").unwrap(),
            from: None,
            to: None,
        }],
        vec![Action::ServiceCall {
            domain: "light".into(),
            service: "turn_on".into(),
            data: serde_json::json!({}),
        }],
    );
    auto.condition = vec![Condition::Template {
        value_template: "{{ is_state('sensor.flag', 'on') }}".into(),
    }];
    engine.register(auto);
    let _handle = engine.start();

    hc.states().set(
        EntityId::parse("switch.trigger").unwrap(),
        "go",
        serde_json::json!({}),
        Context::new(),
    );
    sleep(Duration::from_millis(50)).await;
    assert_eq!(
        log.lock().unwrap().len(),
        1,
        "template condition should evaluate true and let the action run (HC-WS-07)"
    );
}

#[tokio::test]
async fn template_condition_evaluates_false_blocks_action() {
    let hc = HomeCore::new();
    let log = register_recorder(&hc, "light", "turn_on").await;
    hc.states().set(
        EntityId::parse("sensor.flag").unwrap(),
        "off",
        serde_json::json!({}),
        Context::new(),
    );

    let engine = AutomationEngine::new(hc.clone());
    let mut auto = Automation::new(
        "tmpl_auto_false",
        vec![Trigger::State {
            entity_id: EntityId::parse("switch.trigger").unwrap(),
            from: None,
            to: None,
        }],
        vec![Action::ServiceCall {
            domain: "light".into(),
            service: "turn_on".into(),
            data: serde_json::json!({}),
        }],
    );
    auto.condition = vec![Condition::Template {
        value_template: "{{ is_state('sensor.flag', 'on') }}".into(),
    }];
    engine.register(auto);
    let _handle = engine.start();

    hc.states().set(
        EntityId::parse("switch.trigger").unwrap(),
        "go",
        serde_json::json!({}),
        Context::new(),
    );
    sleep(Duration::from_millis(50)).await;
    assert_eq!(log.lock().unwrap().len(), 0, "false template condition should block the action");
}
