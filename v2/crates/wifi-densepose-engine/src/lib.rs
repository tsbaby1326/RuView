//! # RuView Streaming Engine — integration layer
//!
//! This crate is the **composition root** that wires the ADR-135..146 building
//! blocks into one end-to-end *trust-traceable* pipeline cycle. Each block was
//! built and unit-tested independently; this crate proves they compose and that
//! the **trust throughline** holds end-to-end:
//!
//! > *Why believe the system when it says a person is present?* — every
//! > [`TrustedOutput`] names its **signal evidence** (ADR-137 `EvidenceRef`),
//! > its **model version** (ADR-136), its **calibration version** (ADR-135
//! > baseline id, ADR-136 `calibration_id`), and the **privacy decision**
//! > (ADR-141 mode → class) it was emitted under — and is anchored as a
//! > provenance-bearing node in the ADR-139 WorldGraph.
//!
//! One [`StreamingEngine::process_cycle`] performs, in order:
//! 1. **Fuse + score** the node frames (ADR-137 `fuse_scored`) → `QualityScore`
//!    with per-node weights, evidence, and tolerated contradiction flags.
//! 2. **Stamp calibration provenance** (ADR-135/136): the `CalibrationId` the
//!    calibration stage applied is recorded on the `QualityScore`.
//! 3. **Privacy control plane** (ADR-141): if the fusion recorded a tolerated
//!    contradiction, the active privacy class is **demoted one step** before
//!    emission (monotonic — information only ever removed).
//! 4. **Semantic state** (ADR-139/140): a `SemanticState` node is appended to
//!    the WorldGraph with mandatory provenance and a `DerivedFrom` edge to the
//!    room it was observed in.
//!
//! What is intentionally *not* here: the live 20 Hz I/O loop (sensing-server),
//! UWB hardware (ADR-144), and model training (ADR-146). This is the
//! composition + validation layer those will plug into.

#![forbid(unsafe_code)]

use wifi_densepose_bfld::{PrivacyClass, PrivacyMode, PrivacyModeRegistry};
use wifi_densepose_geo::types::GeoRegistration;
use wifi_densepose_signal::ruvsense::fusion_quality::CalibrationId;
use wifi_densepose_signal::ruvsense::multistatic::{MultistaticConfig, MultistaticFuser};
use wifi_densepose_signal::ruvsense::{MultiBandCsiFrame, QualityScore};
use wifi_densepose_worldgraph::{
    EnuPoint, SemanticProvenance, WorldEdge, WorldGraph, WorldId, WorldNode, ZoneBoundsEnu,
};

/// Errors from an engine cycle.
#[derive(Debug)]
pub enum EngineError {
    /// Multistatic fusion failed (no frames, timestamp spread, dimension mismatch).
    Fusion(wifi_densepose_signal::ruvsense::multistatic::MultistaticError),
}

impl core::fmt::Display for EngineError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EngineError::Fusion(e) => write!(f, "fusion error: {e}"),
        }
    }
}
impl std::error::Error for EngineError {}
impl From<wifi_densepose_signal::ruvsense::multistatic::MultistaticError> for EngineError {
    fn from(e: wifi_densepose_signal::ruvsense::multistatic::MultistaticError) -> Self {
        EngineError::Fusion(e)
    }
}

/// The auditable result of one engine cycle — the trust chain made concrete.
#[derive(Debug, Clone)]
pub struct TrustedOutput {
    /// The `SemanticState` node id created in the WorldGraph.
    pub semantic_id: WorldId,
    /// The fusion quality record (evidence + contradictions + calibration).
    pub quality: QualityScore,
    /// The privacy class the output was emitted under (after any demotion).
    pub effective_class: PrivacyClass,
    /// Whether a tolerated contradiction forced a privacy demotion this cycle.
    pub demoted: bool,
    /// The mandatory provenance attached to the semantic node.
    pub provenance: SemanticProvenance,
}

/// Composition root for the RuView streaming engine.
pub struct StreamingEngine {
    fuser: MultistaticFuser,
    coherence_accept: f32,
    privacy: PrivacyModeRegistry,
    world: WorldGraph,
    model_version: u16,
    cycle: u64,
}

impl StreamingEngine {
    /// Build an engine with a starting privacy mode and model version. The
    /// WorldGraph is registered to the installation origin.
    #[must_use]
    pub fn new(mode: PrivacyMode, model_version: u16, registration: GeoRegistration) -> Self {
        Self {
            fuser: MultistaticFuser::with_config(MultistaticConfig::default()),
            coherence_accept: 0.85,
            privacy: PrivacyModeRegistry::new(mode),
            world: WorldGraph::new(registration),
            model_version,
            cycle: 0,
        }
    }

    /// Register a room and return its WorldGraph id (the observation scope).
    pub fn add_room(&mut self, area_id: &str, name: &str) -> WorldId {
        self.world.upsert_node(WorldNode::Room {
            id: WorldId::UNASSIGNED,
            area_id: Some(area_id.to_string()),
            name: name.to_string(),
            bounds_enu: ZoneBoundsEnu::Rectangle { min_e: 0.0, min_n: 0.0, max_e: 5.0, max_n: 4.0 },
            floor: 0,
        })
    }

    /// Register a sensor node and an `observes` edge to a room.
    pub fn add_sensor(&mut self, device_id: &str, room: WorldId) -> WorldId {
        let id = self.world.upsert_node(WorldNode::Sensor {
            id: WorldId::UNASSIGNED,
            device_id: device_id.to_string(),
            position: EnuPoint { east_m: 0.0, north_m: 0.0, up_m: 0.0 },
            modality: wifi_densepose_worldgraph::SensorModality::WifiCsi,
        });
        let _ = self.world.add_edge(
            id,
            room,
            WorldEdge::Observes { quality: 1.0, last_seen_unix_ms: 0 },
        );
        id
    }

    /// Switch the active privacy mode (records a hash-chained attestation).
    pub fn set_privacy_mode(&mut self, mode: PrivacyMode) {
        self.privacy.set_mode(mode);
    }

    /// Borrow the WorldGraph (for queries / persistence).
    #[must_use]
    pub fn world(&self) -> &WorldGraph {
        &self.world
    }

    /// Borrow the privacy registry (for attestation audit).
    #[must_use]
    pub fn privacy(&self) -> &PrivacyModeRegistry {
        &self.privacy
    }

    /// Cycles processed so far.
    #[must_use]
    pub fn cycle_count(&self) -> u64 {
        self.cycle
    }

    /// Run one full trust-traceable cycle (see crate docs for the steps).
    ///
    /// `calibration` is the [`CalibrationId`] the calibration stage applied to
    /// these frames (ADR-135 `BaselineCalibration::calibration_id()`); `room` is
    /// the observation scope (an existing WorldGraph Room id).
    ///
    /// # Errors
    /// [`EngineError::Fusion`] if multistatic fusion rejects the input.
    pub fn process_cycle(
        &mut self,
        node_frames: &[MultiBandCsiFrame],
        calibration: CalibrationId,
        room: WorldId,
        now_ms: i64,
    ) -> Result<TrustedOutput, EngineError> {
        // 1. Fuse + score (ADR-137).
        let (fused, mut quality) = self.fuser.fuse_scored(node_frames, self.coherence_accept)?;

        // 2. Stamp calibration provenance (ADR-135 → ADR-136 → ADR-137).
        quality.calibration_id = Some(calibration);

        // 3. Privacy control plane (ADR-141): demote on contradiction.
        let base_class = self.privacy.active_class();
        let demoted = quality.forces_privacy_demotion();
        let effective_class = if demoted { demote_one(base_class) } else { base_class };

        // 4. Semantic state with mandatory provenance (ADR-139/140).
        let provenance = SemanticProvenance {
            evidence: quality.evidence_refs.iter().map(|e| format!("{e:?}")).collect(),
            model_version: format!("rfenc-v{}", self.model_version),
            calibration_version: format!("cal:{:016x}", calibration.0),
            privacy_decision: format!("{:?}/{:?}", self.privacy.active_mode(), effective_class),
        };
        let statement = format!(
            "occupancy coherence={:.2} nodes={} demoted={}",
            quality.base_coherence, fused.active_nodes, demoted
        );
        let semantic_id = self.world.add_semantic_state(
            statement,
            quality.penalized_coherence(),
            now_ms,
            provenance.clone(),
            &[room],
        );

        self.cycle += 1;
        Ok(TrustedOutput { semantic_id, quality, effective_class, demoted, provenance })
    }
}

/// Demote a privacy class by one step (more restrictive), clamped at `Restricted`.
/// Monotonic: information is only ever removed (ADR-120/141).
fn demote_one(c: PrivacyClass) -> PrivacyClass {
    let next = (c.as_u8() + 1).min(PrivacyClass::Restricted.as_u8());
    PrivacyClass::try_from(next).unwrap_or(PrivacyClass::Restricted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wifi_densepose_signal::hardware_norm::{CanonicalCsiFrame, HardwareType};

    fn node_frame(node_id: u8, ts_us: u64, n_sub: usize) -> MultiBandCsiFrame {
        MultiBandCsiFrame {
            node_id,
            timestamp_us: ts_us,
            channel_frames: vec![CanonicalCsiFrame {
                amplitude: (0..n_sub).map(|i| 1.0 + 0.1 * i as f32).collect(),
                phase: (0..n_sub).map(|i| i as f32 * 0.05).collect(),
                hardware_type: HardwareType::Esp32S3,
            }],
            frequencies_mhz: vec![2412],
            coherence: 0.9,
        }
    }

    fn engine() -> (StreamingEngine, WorldId) {
        let mut e = StreamingEngine::new(PrivacyMode::PrivateHome, 1, GeoRegistration::default());
        let room = e.add_room("living_room", "Living Room");
        e.add_sensor("esp32-com9", room);
        (e, room)
    }

    /// End-to-end trust invariant: a clean cycle produces a SemanticState whose
    /// provenance names evidence + model + calibration + privacy decision, and
    /// the calibration id flows from input → QualityScore → provenance.
    #[test]
    fn cycle_carries_full_provenance() {
        let (mut e, room) = engine();
        let cal = CalibrationId(0xABCD_1234);
        let frames = [node_frame(0, 1000, 56), node_frame(1, 1001, 56)];
        let out = e.process_cycle(&frames, cal, room, 10_000).unwrap();

        // Calibration flows all the way through.
        assert_eq!(out.quality.calibration_id, Some(cal));
        assert_eq!(out.provenance.calibration_version, "cal:00000000abcd1234");
        // Model + privacy provenance present.
        assert_eq!(out.provenance.model_version, "rfenc-v1");
        assert!(out.provenance.privacy_decision.starts_with("PrivateHome/"));
        // Evidence refs recorded.
        assert!(!out.provenance.evidence.is_empty());
        // Clean cycle (tight timestamps) → no demotion, stays Anonymous (PrivateHome).
        assert!(!out.demoted);
        assert_eq!(out.effective_class, PrivacyClass::Anonymous);

        // The SemanticState is in the graph with a DerivedFrom edge to the room.
        assert!(e.world().node(out.semantic_id).is_some());
        assert!(e
            .world()
            .neighbors(out.semantic_id)
            .iter()
            .any(|(to, edge)| *to == room && matches!(edge, WorldEdge::DerivedFrom { .. })));
    }

    /// A tolerated contradiction (loose timestamp spread, within the hard guard)
    /// demotes the privacy class one step — proving ADR-137 → ADR-141 wiring.
    #[test]
    fn contradiction_demotes_privacy() {
        let (mut e, room) = engine();
        let cal = CalibrationId(7);
        // 2 ms spread: within the 5 ms hard guard but above the 1 ms soft guard.
        let frames = [node_frame(0, 1000, 56), node_frame(1, 3000, 56)];
        let out = e.process_cycle(&frames, cal, room, 20_000).unwrap();

        assert!(out.demoted, "loose alignment must demote");
        // PrivateHome base = Anonymous(2) → demoted to Restricted(3).
        assert_eq!(out.effective_class, PrivacyClass::Restricted);
        assert!(out.provenance.privacy_decision.contains("Restricted"));
        // Penalized coherence is below the base coherence.
        assert!(out.quality.penalized_coherence() <= out.quality.base_coherence);
    }

    /// Determinism: identical input twice → identical provenance + class
    /// (the ADR-136 witness-replay spirit, end-to-end through the engine).
    #[test]
    fn cycle_is_deterministic() {
        let cal = CalibrationId(42);
        let frames = [node_frame(0, 1000, 56), node_frame(1, 1001, 56)];

        let (mut e1, r1) = engine();
        let o1 = e1.process_cycle(&frames, cal, r1, 5_000).unwrap();
        let (mut e2, r2) = engine();
        let o2 = e2.process_cycle(&frames, cal, r2, 5_000).unwrap();

        assert_eq!(o1.provenance.calibration_version, o2.provenance.calibration_version);
        assert_eq!(o1.provenance.evidence, o2.provenance.evidence);
        assert_eq!(o1.effective_class, o2.effective_class);
        assert_eq!(o1.quality.per_node_weights, o2.quality.per_node_weights);
    }

    /// The privacy mode switch is recorded in a verifiable attestation chain
    /// (ADR-141), and a stricter mode raises the emitted class.
    #[test]
    fn privacy_mode_switch_is_attested_and_effective() {
        let (mut e, room) = engine();
        e.set_privacy_mode(PrivacyMode::StrictNoIdentity);
        assert!(e.privacy().verify_chain());
        let out = e
            .process_cycle(&[node_frame(0, 1000, 56), node_frame(1, 1001, 56)], CalibrationId(1), room, 1)
            .unwrap();
        // StrictNoIdentity base = Restricted, even with no contradiction.
        assert_eq!(out.effective_class, PrivacyClass::Restricted);
    }
}
