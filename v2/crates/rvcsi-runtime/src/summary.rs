//! One-shot capture operations: summarize a `.rvcsi` file, decode a buffer of
//! napi-c Nexmon records, replay a capture into events, export windows to a
//! JSONL RF-memory file. Everything returns normalized/validated rvCSI types —
//! frames are always run through `validate_frame` and never returned `Pending`
//! or `Rejected` (ADR-095 D6).

use serde::{Deserialize, Serialize};

use rvcsi_adapter_file::{read_all, CaptureHeader};
use rvcsi_adapter_nexmon::NexmonAdapter;
use rvcsi_core::{
    validate_frame, AdapterProfile, CsiEvent, CsiFrame, RvcsiError, SessionId, SourceId,
    ValidationPolicy, ValidationStatus,
};
use rvcsi_dsp::SignalPipeline;
use rvcsi_events::EventPipeline;
use rvcsi_ruvector::{window_embedding, InMemoryRfMemory, JsonlRfMemory, RfMemoryStore};

/// A compact summary of a `.rvcsi` capture file (the `rvcsi inspect` payload /
/// the `inspectCaptureFile` napi return).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureSummary {
    /// The recorded capture format version.
    pub capture_version: u32,
    /// Session id from the header.
    pub session_id: u64,
    /// Source id from the header.
    pub source_id: String,
    /// Adapter kind slug from the header's profile.
    pub adapter_kind: String,
    /// Number of frames in the capture.
    pub frame_count: usize,
    /// First / last frame timestamp (ns); `0` for an empty capture.
    pub first_timestamp_ns: u64,
    /// Last frame timestamp (ns).
    pub last_timestamp_ns: u64,
    /// Distinct WiFi channels seen.
    pub channels: Vec<u16>,
    /// Distinct subcarrier counts seen.
    pub subcarrier_counts: Vec<u16>,
    /// Mean `quality_score` over all frames (`0.0` for an empty capture).
    pub mean_quality: f32,
    /// Count of frames by `ValidationStatus` (`accepted`, `degraded`, `recovered`,
    /// `rejected`, `pending`).
    pub validation_breakdown: ValidationBreakdown,
    /// Calibration version recorded in the header, if any.
    pub calibration_version: Option<String>,
}

/// Per-`ValidationStatus` frame counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationBreakdown {
    /// `ValidationStatus::Pending`
    pub pending: usize,
    /// `ValidationStatus::Accepted`
    pub accepted: usize,
    /// `ValidationStatus::Degraded`
    pub degraded: usize,
    /// `ValidationStatus::Rejected`
    pub rejected: usize,
    /// `ValidationStatus::Recovered`
    pub recovered: usize,
}

impl ValidationBreakdown {
    fn tally(&mut self, s: ValidationStatus) {
        match s {
            ValidationStatus::Pending => self.pending += 1,
            ValidationStatus::Accepted => self.accepted += 1,
            ValidationStatus::Degraded => self.degraded += 1,
            ValidationStatus::Rejected => self.rejected += 1,
            ValidationStatus::Recovered => self.recovered += 1,
        }
    }
}

fn sorted_unique<T: Ord + Copy>(mut v: Vec<T>) -> Vec<T> {
    v.sort_unstable();
    v.dedup();
    v
}

/// Summarize a `.rvcsi` capture file.
pub fn summarize_capture(path: &str) -> Result<CaptureSummary, RvcsiError> {
    let (header, frames): (CaptureHeader, Vec<CsiFrame>) = read_all(path)?;
    let mut channels = Vec::new();
    let mut subcarrier_counts = Vec::new();
    let mut breakdown = ValidationBreakdown::default();
    let mut quality_sum = 0.0f32;
    let (mut first_ts, mut last_ts) = (u64::MAX, 0u64);
    for f in &frames {
        channels.push(f.channel);
        subcarrier_counts.push(f.subcarrier_count);
        breakdown.tally(f.validation);
        quality_sum += f.quality_score;
        first_ts = first_ts.min(f.timestamp_ns);
        last_ts = last_ts.max(f.timestamp_ns);
    }
    if frames.is_empty() {
        first_ts = 0;
    }
    Ok(CaptureSummary {
        capture_version: header.rvcsi_capture_version,
        session_id: header.session_id.value(),
        source_id: header.source_id.0,
        adapter_kind: header.adapter_profile.adapter_kind.slug().to_string(),
        frame_count: frames.len(),
        first_timestamp_ns: first_ts,
        last_timestamp_ns: last_ts,
        channels: sorted_unique(channels),
        subcarrier_counts: sorted_unique(subcarrier_counts),
        mean_quality: if frames.is_empty() {
            0.0
        } else {
            quality_sum / frames.len() as f32
        },
        validation_breakdown: breakdown,
        calibration_version: header.calibration_version,
    })
}

/// Decode a buffer of "rvCSI Nexmon records" (the napi-c shim format) into
/// validated [`CsiFrame`]s. Each frame is run through [`validate_frame`] against
/// a permissive profile (so synthetic / non-default subcarrier counts survive);
/// frames that hard-fail validation are dropped (never returned to JS).
pub fn decode_nexmon_records(
    bytes: &[u8],
    source_id: &str,
    session_id: u64,
) -> Result<Vec<CsiFrame>, RvcsiError> {
    let raw = NexmonAdapter::frames_from_bytes(SourceId::from(source_id), SessionId(session_id), bytes)?;
    let profile = AdapterProfile::offline(rvcsi_core::AdapterKind::Nexmon);
    let policy = ValidationPolicy::default();
    let mut out = Vec::with_capacity(raw.len());
    let mut prev_ts: Option<u64> = None;
    for mut f in raw {
        let ts = f.timestamp_ns;
        match validate_frame(&mut f, &profile, &policy, prev_ts) {
            Ok(()) => {
                if f.is_exposable() {
                    prev_ts = Some(ts);
                    out.push(f);
                }
            }
            Err(_) => { /* hard-rejected — dropped, not returned to JS */ }
        }
    }
    Ok(out)
}

/// Replay a `.rvcsi` capture through the DSP + event pipeline and collect every
/// emitted [`CsiEvent`]. Frames that arrive `Pending` are validated first;
/// already-validated frames are trusted (replay fidelity).
pub fn events_from_capture(path: &str) -> Result<Vec<CsiEvent>, RvcsiError> {
    let (header, frames) = read_all(path)?;
    let dsp = SignalPipeline::default();
    let mut pipeline = EventPipeline::with_defaults(header.session_id, header.source_id.clone());
    let profile = header.adapter_profile.clone();
    let policy = header.validation_policy.clone();
    let mut prev_ts: Option<u64> = None;
    let mut events = Vec::new();
    for mut f in frames {
        if f.validation == ValidationStatus::Pending {
            let ts = f.timestamp_ns;
            if validate_frame(&mut f, &profile, &policy, prev_ts).is_err() || !f.is_exposable() {
                continue;
            }
            prev_ts = Some(ts);
        }
        dsp.process_frame(&mut f);
        events.extend(pipeline.process_frame(&f));
    }
    events.extend(pipeline.flush());
    Ok(events)
}

/// Replay a `.rvcsi` capture, window it, and store every window's embedding into
/// a JSONL RF-memory file (the `rvcsi export ruvector` payload). Returns the
/// number of windows stored.
pub fn export_capture_to_rf_memory(capture_path: &str, out_jsonl_path: &str) -> Result<usize, RvcsiError> {
    let (header, frames) = read_all(capture_path)?;
    let mut pipeline = EventPipeline::with_defaults(header.session_id, header.source_id.clone());
    let dsp = SignalPipeline::default();
    let mut store = JsonlRfMemory::create(out_jsonl_path)?;
    let mut stored = 0usize;
    for mut f in frames {
        if !f.is_exposable() {
            continue;
        }
        dsp.process_frame(&mut f);
        let _ = pipeline.process_frame(&f);
    }
    let _ = pipeline.flush();
    for w in pipeline.recent_windows() {
        store.store_window(w)?;
        stored += 1;
    }
    Ok(stored)
}

/// Convenience used by tests / examples: window a capture in memory and return
/// `(window_count, top_self_similarity)` — storing each window then querying
/// with the first window's embedding should yield itself with score ≈ 1.0.
pub fn rf_memory_self_check(capture_path: &str) -> Result<(usize, f32), RvcsiError> {
    let (header, frames) = read_all(capture_path)?;
    let mut pipeline = EventPipeline::with_defaults(header.session_id, header.source_id.clone());
    for f in &frames {
        if f.is_exposable() {
            let _ = pipeline.process_frame(f);
        }
    }
    let _ = pipeline.flush();
    let windows: Vec<_> = pipeline.recent_windows().to_vec();
    let mut store = InMemoryRfMemory::new();
    for w in &windows {
        store.store_window(w)?;
    }
    if windows.is_empty() {
        return Ok((0, 0.0));
    }
    let q = window_embedding(&windows[0]);
    let hits = store.query_similar(&q, 1)?;
    Ok((windows.len(), hits.first().map(|h| h.score).unwrap_or(0.0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rvcsi_adapter_file::FileRecorder;
    use rvcsi_adapter_nexmon::{encode_record, NexmonRecord};
    use rvcsi_core::{AdapterKind, FrameId};

    fn write_capture(path: &std::path::Path, n: usize) {
        let header = CaptureHeader::new(
            SessionId(1),
            SourceId::from("it"),
            AdapterProfile::offline(AdapterKind::File),
        );
        let mut rec = FileRecorder::create(path, &header).unwrap();
        for k in 0..n {
            // alternate "quiet" and "active" amplitudes so the event pipeline has something to do
            let amp_scale = if (k / 8) % 2 == 0 { 0.0 } else { 1.5 };
            let i: Vec<f32> = (0..32).map(|s| 1.0 + amp_scale * (((k + s) % 5) as f32 - 2.0)).collect();
            let q: Vec<f32> = (0..32).map(|s| 0.5 + amp_scale * (((k * 3 + s) % 7) as f32 - 3.0) * 0.1).collect();
            let mut f = CsiFrame::from_iq(
                FrameId(k as u64),
                SessionId(1),
                SourceId::from("it"),
                AdapterKind::File,
                1_000 + k as u64 * 50_000_000, // 50 ms apart
                6,
                20,
                i,
                q,
            )
            .with_rssi(-55);
            f.validation = ValidationStatus::Accepted;
            f.quality_score = 0.9;
            rec.write_frame(&f).unwrap();
        }
        rec.finish().unwrap();
    }

    #[test]
    fn summarize_a_recorded_capture() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_capture(tmp.path(), 10);
        let s = summarize_capture(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(s.capture_version, 1);
        assert_eq!(s.session_id, 1);
        assert_eq!(s.frame_count, 10);
        assert_eq!(s.channels, vec![6]);
        assert_eq!(s.subcarrier_counts, vec![32]);
        assert_eq!(s.validation_breakdown.accepted, 10);
        assert!((s.mean_quality - 0.9).abs() < 1e-5);
        assert_eq!(s.first_timestamp_ns, 1_000);
        assert!(s.last_timestamp_ns > s.first_timestamp_ns);
    }

    #[test]
    fn summarize_empty_capture() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let header = CaptureHeader::new(SessionId(9), SourceId::from("e"), AdapterProfile::offline(AdapterKind::File));
        FileRecorder::create(tmp.path(), &header).unwrap().finish().unwrap();
        let s = summarize_capture(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(s.frame_count, 0);
        assert_eq!(s.mean_quality, 0.0);
        assert_eq!(s.first_timestamp_ns, 0);
    }

    #[test]
    fn decode_nexmon_records_validates_and_returns_frames() {
        // two 64-subcarrier records
        let mk = |ts: u64, rssi: i16| {
            let rec = NexmonRecord {
                subcarrier_count: 64,
                channel: 36,
                bandwidth_mhz: 80,
                rssi_dbm: Some(rssi),
                noise_floor_dbm: Some(-92),
                timestamp_ns: ts,
                i_values: (0..64).map(|k| (k as f32) * 0.25).collect(),
                q_values: (0..64).map(|k| -(k as f32) * 0.1).collect(),
            };
            encode_record(&rec).unwrap()
        };
        let mut buf = mk(1_000, -58);
        buf.extend(mk(2_000, -59));
        let frames = decode_nexmon_records(&buf, "nexmon-test", 7).unwrap();
        assert_eq!(frames.len(), 2);
        for f in &frames {
            assert!(f.is_exposable());
            assert_eq!(f.subcarrier_count, 64);
            assert_eq!(f.adapter_kind, AdapterKind::Nexmon);
        }
        assert_eq!(frames[1].timestamp_ns, 2_000);
    }

    #[test]
    fn events_and_export_from_capture() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_capture(tmp.path(), 64);
        let events = events_from_capture(tmp.path().to_str().unwrap()).unwrap();
        // the alternating quiet/active stream should produce at least one event,
        // and every event must be well-formed.
        assert!(!events.is_empty(), "expected the event pipeline to emit something");
        for e in &events {
            e.validate().unwrap();
            assert!((0.0..=1.0).contains(&e.confidence));
            assert!(!e.evidence_window_ids.is_empty());
        }

        let out = tempfile::NamedTempFile::new().unwrap();
        let stored = export_capture_to_rf_memory(
            tmp.path().to_str().unwrap(),
            out.path().to_str().unwrap(),
        )
        .unwrap();
        assert!(stored > 0);
        // re-open the JSONL store and confirm the records round-tripped
        let reopened = JsonlRfMemory::open(out.path().to_str().unwrap()).unwrap();
        assert_eq!(reopened.len(), stored);

        let (wc, score) = rf_memory_self_check(tmp.path().to_str().unwrap()).unwrap();
        assert!(wc > 0);
        assert!((score - 1.0).abs() < 1e-4, "self-similarity should be ~1.0, got {score}");
    }

    #[test]
    fn missing_capture_file_is_a_structured_error() {
        assert!(summarize_capture("/nonexistent/path/x.rvcsi").is_err());
        assert!(events_from_capture("/nonexistent/path/x.rvcsi").is_err());
    }
}
