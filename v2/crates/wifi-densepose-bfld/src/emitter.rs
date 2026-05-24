//! `BfldEmitter` — end-to-end pipeline. ADR-118 §2.1.
//!
//! Wires the per-frame sensing inputs through:
//!
//! ```text
//!  risk = identity_risk::score(sep, stab, consist, conf_factor)
//!    -> gate.evaluate_with_oracle(risk, ts, &oracle) -> GateAction
//!       -> if Recalibrate: ring.drain()
//!       -> if action.drops_event(): return None
//!       -> else: BfldEvent::with_privacy_gating(...)
//! ```
//!
//! The emitter owns the `CoherenceGate` and `EmbeddingRing` state so the
//! caller only supplies per-frame inputs. Identity embeddings are pushed to
//! the ring before the gate is consulted; on `Recalibrate` the ring is
//! drained synchronously inside this function.

#![cfg(feature = "std")]

use crate::coherence_gate::{CoherenceGate, NullOracle, SoulMatchOracle};
use crate::embedding_ring::EmbeddingRing;
use crate::identity_risk::{score, GateAction};
use crate::{BfldEvent, IdentityEmbedding, PrivacyClass};

/// Per-frame sensing inputs to [`BfldEmitter::emit`].
#[derive(Debug, Clone)]
pub struct SensingInputs {
    /// Monotonic capture-clock timestamp in nanoseconds.
    pub timestamp_ns: u64,
    /// Whether an occupant is present in the zone.
    pub presence: bool,
    /// Normalized motion magnitude `[0,1]`.
    pub motion: f32,
    /// Estimated occupant count.
    pub person_count: u8,
    /// Sensing confidence (NOT the risk-score `conf` factor) — `[0,1]`.
    pub sensing_confidence: f32,

    // --- Risk-score factors (ADR-121 §2.2) -------------------------------
    /// `identity_separability_score` — `[0,1]`.
    pub sep: f32,
    /// `temporal_stability` — `[0,1]`.
    pub stab: f32,
    /// `cross_perspective_consistency` — `[0,1]`.
    pub consist: f32,
    /// Risk-score sample confidence factor — `[0,1]`.
    pub risk_conf: f32,

    // --- Optional identity-derived fields --------------------------------
    /// Per-day BLAKE3-keyed `rf_signature_hash`. Stripped at class 3 by the
    /// privacy-gated event constructor.
    pub rf_signature_hash: Option<[u8; 32]>,
}

/// End-to-end pipeline. Owns the gate state, the embedding ring, and the
/// configured node identity. Defaults to `PrivacyClass::Anonymous`.
pub struct BfldEmitter {
    node_id: String,
    default_zone_id: Option<String>,
    privacy_class: PrivacyClass,
    gate: CoherenceGate,
    ring: EmbeddingRing,
}

impl BfldEmitter {
    /// Build a new emitter in the production-default state: class Anonymous,
    /// empty gate/ring, no default zone.
    #[must_use]
    pub fn new(node_id: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            default_zone_id: None,
            privacy_class: PrivacyClass::Anonymous,
            gate: CoherenceGate::new(),
            ring: EmbeddingRing::new(),
        }
    }

    /// Set the default zone ID emitted with each event (None = single-zone).
    #[must_use]
    pub fn with_zone(mut self, zone_id: impl Into<String>) -> Self {
        self.default_zone_id = Some(zone_id.into());
        self
    }

    /// Override the privacy class (default `Anonymous`).
    #[must_use]
    pub const fn with_privacy_class(mut self, class: PrivacyClass) -> Self {
        self.privacy_class = class;
        self
    }

    /// Read-only access to the current gate action — useful for diagnostics.
    #[must_use]
    pub const fn current_action(&self) -> GateAction {
        self.gate.current()
    }

    /// Read-only access to the ring length (post any in-flight drain).
    #[must_use]
    pub const fn ring_len(&self) -> usize {
        self.ring.len()
    }

    /// Run one pipeline step with the default [`NullOracle`]. Returns
    /// `Some(BfldEvent)` if the gate permitted publishing, `None` if the
    /// action was `Reject` or `Recalibrate`.
    pub fn emit(
        &mut self,
        inputs: SensingInputs,
        embedding: Option<IdentityEmbedding>,
    ) -> Option<BfldEvent> {
        self.emit_with_oracle(inputs, embedding, &NullOracle)
    }

    /// Same as [`Self::emit`] but consults a [`SoulMatchOracle`] before the
    /// gate fires `Recalibrate`. See ADR-121 §2.6.
    pub fn emit_with_oracle<O: SoulMatchOracle>(
        &mut self,
        inputs: SensingInputs,
        embedding: Option<IdentityEmbedding>,
        oracle: &O,
    ) -> Option<BfldEvent> {
        let risk = score(inputs.sep, inputs.stab, inputs.consist, inputs.risk_conf);

        if let Some(emb) = embedding {
            // Always push, regardless of action — the ring is the rolling
            // memory of recent identity embeddings, used for separability.
            self.ring.push(emb);
        }

        let action = self
            .gate
            .evaluate_with_oracle(risk, inputs.timestamp_ns, oracle);

        if action == GateAction::Recalibrate {
            self.ring.drain();
        }

        if action.drops_event() {
            return None;
        }

        let identity_risk_score = match self.privacy_class {
            PrivacyClass::Anonymous => Some(risk),
            // Class 3 strips identity_risk; class 0/1 keep it (research modes).
            // The BfldEvent constructor enforces the class-3 strip again as a
            // defense-in-depth measure.
            _ => Some(risk),
        };

        Some(BfldEvent::with_privacy_gating(
            self.node_id.clone(),
            inputs.timestamp_ns,
            inputs.presence,
            inputs.motion,
            inputs.person_count,
            inputs.sensing_confidence,
            self.default_zone_id.clone(),
            self.privacy_class,
            identity_risk_score,
            inputs.rf_signature_hash,
        ))
    }
}
