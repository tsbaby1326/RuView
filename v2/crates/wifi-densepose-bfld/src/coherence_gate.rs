//! Stateful coherence gate with hysteresis + debounce. ADR-121 §2.4 + §2.5.
//!
//! Wraps the stateless [`crate::identity_risk::GateAction::from_score`] band
//! classifier with two stabilizing mechanisms:
//!
//! - **Hysteresis (±0.05)** — a score must clear the current band's edge by
//!   `HYSTERESIS` before the gate considers the next band.
//! - **Debounce (5 seconds)** — once a different action is "pending", it must
//!   persist for `DEBOUNCE_NS` of wall time before it becomes the current
//!   action. Returning to the current band cancels the pending action.
//!
//! Together these prevent the gate from flapping when the risk score
//! oscillates near a boundary or spikes briefly on a single bad frame.

use crate::identity_risk::{
    GateAction, PREDICT_ONLY_THRESHOLD, RECALIBRATE_THRESHOLD, REJECT_THRESHOLD,
};

/// Symmetric hysteresis band applied to every action boundary.
pub const HYSTERESIS: f32 = 0.05;

/// Pending action must persist this long (in nanoseconds) before promotion.
pub const DEBOUNCE_NS: u64 = 5_000_000_000;

/// Stateful gate. Construct with `CoherenceGate::new()` and call
/// `evaluate(score, timestamp_ns)` per frame to obtain the active action.
pub struct CoherenceGate {
    current: GateAction,
    pending: Option<(GateAction, u64)>,
}

impl CoherenceGate {
    /// Build a fresh gate, starting in [`GateAction::Accept`] with no pending
    /// transition.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current: GateAction::Accept,
            pending: None,
        }
    }

    /// Current published action — does **not** advance any state.
    #[must_use]
    pub const fn current(&self) -> GateAction {
        self.current
    }

    /// Pending action (if any) — useful for diagnostics / dashboards.
    #[must_use]
    pub const fn pending(&self) -> Option<GateAction> {
        match self.pending {
            Some((a, _)) => Some(a),
            None => None,
        }
    }

    /// Drive the gate with a fresh score reading and a monotonic timestamp.
    /// Returns the currently-active action after the update.
    pub fn evaluate(&mut self, score: f32, timestamp_ns: u64) -> GateAction {
        let target = effective_target(score, self.current);
        if target == self.current {
            // Score is back inside (or never left) the current band's hysteresis
            // envelope. Cancel any pending transition.
            self.pending = None;
            return self.current;
        }
        match self.pending {
            Some((pending, since)) if pending == target => {
                // Same target as before — check whether debounce has elapsed.
                if timestamp_ns.saturating_sub(since) >= DEBOUNCE_NS {
                    self.current = target;
                    self.pending = None;
                }
            }
            _ => {
                // Either no pending, or pending differs from current target.
                self.pending = Some((target, timestamp_ns));
            }
        }
        self.current
    }
}

impl Default for CoherenceGate {
    fn default() -> Self {
        Self::new()
    }
}

fn effective_target(score: f32, current: GateAction) -> GateAction {
    let raw = GateAction::from_score(score);
    if raw == current {
        return current;
    }
    if action_idx(raw) > action_idx(current) {
        // Crossing upward — score must clear current's upper edge + HYSTERESIS.
        if score >= upper_edge_of(current) + HYSTERESIS {
            raw
        } else {
            current
        }
    } else {
        // Crossing downward — score must fall below current's lower edge - HYSTERESIS.
        if score < lower_edge_of(current) - HYSTERESIS {
            raw
        } else {
            current
        }
    }
}

const fn action_idx(a: GateAction) -> u8 {
    match a {
        GateAction::Accept => 0,
        GateAction::PredictOnly => 1,
        GateAction::Reject => 2,
        GateAction::Recalibrate => 3,
    }
}

fn upper_edge_of(a: GateAction) -> f32 {
    match a {
        GateAction::Accept => PREDICT_ONLY_THRESHOLD,
        GateAction::PredictOnly => REJECT_THRESHOLD,
        GateAction::Reject => RECALIBRATE_THRESHOLD,
        GateAction::Recalibrate => f32::INFINITY,
    }
}

fn lower_edge_of(a: GateAction) -> f32 {
    match a {
        GateAction::Accept => f32::NEG_INFINITY,
        GateAction::PredictOnly => PREDICT_ONLY_THRESHOLD,
        GateAction::Reject => REJECT_THRESHOLD,
        GateAction::Recalibrate => RECALIBRATE_THRESHOLD,
    }
}
