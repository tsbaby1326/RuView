//! The per-room specialist bank (ADR-151 Stage 4).
//!
//! A versioned collection of small models scoped to one `room_id`, fit from the
//! enrollment anchors and tied to the ADR-135 baseline it was trained against.
//! When the baseline drifts (room rearranged, AP moved), the bank is marked
//! STALE rather than emitting confident-but-wrong readings — the calibration
//! analogue of the firmware's honest `DEGRADED` flag.

use serde::{Deserialize, Serialize};

use crate::error::{CalibrationError, Result};
use crate::extract::AnchorFeature;
use crate::specialist::{
    AnomalySpecialist, BreathingSpecialist, HeartbeatSpecialist, PostureSpecialist,
    PresenceSpecialist, RestlessnessSpecialist, SpecialistKind,
};

/// A versioned bank of room-calibrated specialists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecialistBank {
    /// Room scope.
    pub room_id: String,
    /// ADR-135 baseline id this bank was trained against (drift → STALE).
    pub baseline_id: String,
    /// Training time (unix seconds).
    pub trained_at_unix_s: i64,
    /// Number of anchors used.
    pub anchor_count: usize,

    /// Presence gate (requires the `empty` + an occupied anchor).
    pub presence: Option<PresenceSpecialist>,
    /// Posture classifier (requires posture anchors).
    pub posture: Option<PostureSpecialist>,
    /// Breathing (band-limited periodicity; stateless).
    pub breathing: BreathingSpecialist,
    /// Heartbeat (band-limited periodicity; stateless).
    pub heartbeat: HeartbeatSpecialist,
    /// Restlessness (requires calm + active anchors).
    pub restlessness: Option<RestlessnessSpecialist>,
    /// Anomaly novelty detector (requires ≥2 anchors).
    pub anomaly: Option<AnomalySpecialist>,
}

impl SpecialistBank {
    /// Train a bank from enrollment anchor features.
    ///
    /// Requires at least one anchor; specialists whose prerequisite anchors are
    /// missing are simply left `None` (a partial bank still works for the
    /// signals it could fit).
    pub fn train(
        room_id: impl Into<String>,
        baseline_id: impl Into<String>,
        anchors: &[AnchorFeature],
        at_unix_s: i64,
    ) -> Result<Self> {
        if anchors.is_empty() {
            return Err(CalibrationError::InsufficientSamples {
                kind: "bank".into(),
                have: 0,
                need: 1,
            });
        }
        Ok(Self {
            room_id: room_id.into(),
            baseline_id: baseline_id.into(),
            trained_at_unix_s: at_unix_s,
            anchor_count: anchors.len(),
            presence: PresenceSpecialist::train(anchors),
            posture: PostureSpecialist::train(anchors),
            breathing: BreathingSpecialist::default(),
            heartbeat: HeartbeatSpecialist::default(),
            restlessness: RestlessnessSpecialist::train(anchors),
            anomaly: AnomalySpecialist::train(anchors),
        })
    }

    /// `true` if the bank was trained against a different baseline (it is STALE).
    pub fn is_stale(&self, current_baseline_id: &str) -> bool {
        self.baseline_id != current_baseline_id
    }

    /// Error out if stale.
    pub fn check_fresh(&self, current_baseline_id: &str) -> Result<()> {
        if self.is_stale(current_baseline_id) {
            Err(CalibrationError::StaleBaseline {
                trained: self.baseline_id.clone(),
                current: current_baseline_id.to_string(),
            })
        } else {
            Ok(())
        }
    }

    /// Which specialists were successfully fit.
    pub fn trained_kinds(&self) -> Vec<SpecialistKind> {
        let mut v = vec![SpecialistKind::Breathing, SpecialistKind::Heartbeat];
        if self.presence.is_some() {
            v.push(SpecialistKind::Presence);
        }
        if self.posture.is_some() {
            v.push(SpecialistKind::Posture);
        }
        if self.restlessness.is_some() {
            v.push(SpecialistKind::Restlessness);
        }
        if self.anomaly.is_some() {
            v.push(SpecialistKind::Anomaly);
        }
        v
    }

    /// Serialize to JSON.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|e| CalibrationError::Serde(e.to_string()))
    }

    /// Deserialize from JSON.
    pub fn from_json(s: &str) -> Result<Self> {
        serde_json::from_str(s).map_err(|e| CalibrationError::Serde(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::AnchorLabel;
    use crate::extract::Features;

    fn af(label: AnchorLabel, variance: f32, motion: f32) -> AnchorFeature {
        AnchorFeature {
            room_id: "living-room".into(),
            label,
            features: Features {
                mean: 1.0,
                variance,
                motion,
                breathing_score: 0.0,
                breathing_hz: 0.0,
                heart_score: 0.0,
                heart_hz: 0.0,
            },
        }
    }

    fn full_anchors() -> Vec<AnchorFeature> {
        vec![
            af(AnchorLabel::Empty, 1.0, 0.1),
            af(AnchorLabel::StandStill, 10.0, 0.2),
            af(AnchorLabel::Sit, 6.0, 0.2),
            af(AnchorLabel::LieDown, 3.0, 0.2),
            af(AnchorLabel::SmallMove, 4.0, 1.2),
            af(AnchorLabel::SleepPosture, 3.0, 0.1),
        ]
    }

    #[test]
    fn train_full_bank() {
        let bank = SpecialistBank::train("living-room", "base-1", &full_anchors(), 1000).unwrap();
        let kinds = bank.trained_kinds();
        assert!(kinds.contains(&SpecialistKind::Presence));
        assert!(kinds.contains(&SpecialistKind::Posture));
        assert!(kinds.contains(&SpecialistKind::Restlessness));
        assert!(kinds.contains(&SpecialistKind::Anomaly));
        assert_eq!(bank.anchor_count, 6);
    }

    #[test]
    fn empty_anchors_error() {
        assert!(SpecialistBank::train("r", "b", &[], 0).is_err());
    }

    #[test]
    fn json_roundtrip() {
        let bank = SpecialistBank::train("r", "base-1", &full_anchors(), 1000).unwrap();
        let json = bank.to_json().unwrap();
        let back = SpecialistBank::from_json(&json).unwrap();
        assert_eq!(back.room_id, "r");
        assert_eq!(back.anchor_count, 6);
    }

    #[test]
    fn staleness() {
        let bank = SpecialistBank::train("r", "base-1", &full_anchors(), 1000).unwrap();
        assert!(!bank.is_stale("base-1"));
        assert!(bank.is_stale("base-2"));
        assert!(bank.check_fresh("base-2").is_err());
    }
}
