//! # rvCSI Nexmon adapter (napi-c boundary)
//!
//! Wraps the isolated C shim in `native/rvcsi_nexmon_shim.{c,h}` — the only C
//! in the rvCSI runtime (ADR-095 D2, ADR-096). The shim parses a compact,
//! byte-defined "rvCSI Nexmon record" (a normalized superset of the nexmon_csi
//! UDP payload). Everything above [`ffi`] is safe Rust; all `unsafe` is
//! confined to this crate, bounds-checked on the C side, and documented.
//!
//! Typical use: a capture dump (a file of concatenated records, or the bytes of
//! a UDP stream demux) is fed to [`NexmonAdapter::from_bytes`], which yields
//! `Pending` [`CsiFrame`]s. The runtime then runs [`rvcsi_core::validate_frame`]
//! on each before exposing it.

#![warn(missing_docs)]

use std::path::Path;

use rvcsi_core::{
    AdapterKind, AdapterProfile, CsiFrame, CsiSource, RvcsiError, SessionId, SourceHealth, SourceId,
};

pub mod ffi;

pub use ffi::{decode_record, encode_record, shim_abi_version, NexmonRecord, RECORD_HEADER_BYTES};

/// A [`CsiSource`] that replays a buffer of rvCSI Nexmon records.
///
/// Records are decoded lazily by [`CsiSource::next_frame`]; an exhausted buffer
/// returns `Ok(None)`. Frames are produced with `validation = Pending`.
pub struct NexmonAdapter {
    source_id: SourceId,
    session_id: SessionId,
    profile: AdapterProfile,
    buf: Vec<u8>,
    cursor: usize,
    next_frame_id: u64,
    delivered: u64,
    rejected: u64,
    status: Option<String>,
}

impl NexmonAdapter {
    /// Build an adapter from a buffer of concatenated records.
    pub fn from_bytes(
        source_id: impl Into<SourceId>,
        session_id: SessionId,
        bytes: impl Into<Vec<u8>>,
    ) -> Self {
        // ABI guard — the static lib we linked must match the header we coded against.
        debug_assert_eq!(
            shim_abi_version() >> 16,
            1,
            "rvcsi_nexmon_shim major ABI mismatch"
        );
        NexmonAdapter {
            source_id: source_id.into(),
            session_id,
            profile: AdapterProfile::nexmon_default(),
            buf: bytes.into(),
            cursor: 0,
            next_frame_id: 0,
            delivered: 0,
            rejected: 0,
            status: None,
        }
    }

    /// Build an adapter from a capture file of concatenated records.
    pub fn from_file(
        source_id: impl Into<SourceId>,
        session_id: SessionId,
        path: impl AsRef<Path>,
    ) -> Result<Self, RvcsiError> {
        let bytes = std::fs::read(path)?;
        Ok(Self::from_bytes(source_id, session_id, bytes))
    }

    /// Override the capability profile (e.g. when the firmware version is known).
    pub fn with_profile(mut self, profile: AdapterProfile) -> Self {
        self.profile = profile;
        self
    }

    /// Decode every record in `bytes` into `Pending` frames in one shot.
    ///
    /// Stops at the first malformed record and returns what was decoded so far
    /// alongside the error (`Err` carries the partial vec via the message; use
    /// [`NexmonAdapter`] iteration if you need to inspect partial progress).
    pub fn frames_from_bytes(
        source_id: impl Into<SourceId>,
        session_id: SessionId,
        bytes: &[u8],
    ) -> Result<Vec<CsiFrame>, RvcsiError> {
        let mut adapter = NexmonAdapter::from_bytes(source_id, session_id, bytes.to_vec());
        let mut out = Vec::new();
        while let Some(frame) = adapter.next_frame()? {
            out.push(frame);
        }
        Ok(out)
    }

    fn record_to_frame(&mut self, rec: NexmonRecord) -> CsiFrame {
        let fid = self.next_frame_id;
        self.next_frame_id += 1;
        let mut frame = CsiFrame::from_iq(
            fid.into(),
            self.session_id,
            self.source_id.clone(),
            AdapterKind::Nexmon,
            rec.timestamp_ns,
            rec.channel,
            rec.bandwidth_mhz,
            rec.i_values,
            rec.q_values,
        );
        if let Some(r) = rec.rssi_dbm {
            frame.rssi_dbm = Some(r);
        }
        if let Some(n) = rec.noise_floor_dbm {
            frame.noise_floor_dbm = Some(n);
        }
        frame
    }
}

impl CsiSource for NexmonAdapter {
    fn profile(&self) -> &AdapterProfile {
        &self.profile
    }

    fn session_id(&self) -> SessionId {
        self.session_id
    }

    fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    fn next_frame(&mut self) -> Result<Option<CsiFrame>, RvcsiError> {
        if self.cursor >= self.buf.len() {
            return Ok(None);
        }
        let remaining = &self.buf[self.cursor..];
        match decode_record(remaining) {
            Ok((rec, consumed)) => {
                self.cursor += consumed;
                self.delivered += 1;
                Ok(Some(self.record_to_frame(rec)))
            }
            Err(e) => {
                self.rejected += 1;
                self.status = Some(format!("malformed record at byte {}: {e}", self.cursor));
                // Skip the rest of the buffer — a corrupt record means we've lost
                // framing; the daemon would reconnect/re-sync rather than guess.
                self.cursor = self.buf.len();
                Err(RvcsiError::adapter(
                    "nexmon",
                    format!("malformed record: {e}"),
                ))
            }
        }
    }

    fn health(&self) -> SourceHealth {
        SourceHealth {
            connected: self.cursor < self.buf.len(),
            frames_delivered: self.delivered,
            frames_rejected: self.rejected,
            status: self.status.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rvcsi_core::{validate_frame, ValidationPolicy, ValidationStatus};

    fn make_record(ts: u64, ch: u16, n: usize, rssi: Option<i16>) -> Vec<u8> {
        let i: Vec<f32> = (0..n).map(|k| (k as f32) * 0.5).collect();
        let q: Vec<f32> = (0..n).map(|k| -(k as f32) * 0.25).collect();
        let rec = NexmonRecord {
            subcarrier_count: n as u16,
            channel: ch,
            bandwidth_mhz: 80,
            rssi_dbm: rssi,
            noise_floor_dbm: Some(-92),
            timestamp_ns: ts,
            i_values: i,
            q_values: q,
        };
        encode_record(&rec).expect("encode")
    }

    #[test]
    fn abi_version_is_one_point_oh() {
        assert_eq!(shim_abi_version(), 0x0001_0000);
    }

    #[test]
    fn roundtrip_single_record_via_c_shim() {
        let bytes = make_record(123_456, 36, 64, Some(-58));
        let (rec, consumed) = decode_record(&bytes).expect("decode");
        assert_eq!(consumed, bytes.len());
        assert_eq!(rec.subcarrier_count, 64);
        assert_eq!(rec.channel, 36);
        assert_eq!(rec.bandwidth_mhz, 80);
        assert_eq!(rec.rssi_dbm, Some(-58));
        assert_eq!(rec.noise_floor_dbm, Some(-92));
        assert_eq!(rec.timestamp_ns, 123_456);
        assert_eq!(rec.i_values.len(), 64);
        // Q8.8 fixed point: 0.5 and -0.25 are exactly representable.
        assert_eq!(rec.i_values[1], 0.5);
        assert_eq!(rec.q_values[1], -0.25);
    }

    #[test]
    fn adapter_streams_multiple_records_then_validates() {
        let mut buf = make_record(1_000, 6, 56, Some(-60));
        buf.extend(make_record(2_000, 6, 56, Some(-61)));
        buf.extend(make_record(3_000, 6, 56, None));

        let mut adapter = NexmonAdapter::from_bytes("nexmon-test", SessionId(7), buf);
        let mut frames = Vec::new();
        while let Some(f) = adapter.next_frame().unwrap() {
            frames.push(f);
        }
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].timestamp_ns, 1_000);
        assert_eq!(frames[2].rssi_dbm, None);
        assert_eq!(adapter.health().frames_delivered, 3);
        assert!(!adapter.health().connected);

        // 56 is not in the default Nexmon profile (64/128/256) → rejected.
        let mut f = frames[0].clone();
        let err = validate_frame(&mut f, adapter.profile(), &ValidationPolicy::default(), None);
        assert!(err.is_err());

        // With a permissive profile it validates fine.
        let mut f = frames[0].clone();
        validate_frame(
            &mut f,
            &AdapterProfile::offline(AdapterKind::Nexmon),
            &ValidationPolicy::default(),
            None,
        )
        .unwrap();
        assert_eq!(f.validation, ValidationStatus::Accepted);
    }

    #[test]
    fn truncated_buffer_is_a_structured_error_not_a_panic() {
        let bytes = make_record(1, 6, 64, Some(-60));
        let truncated = &bytes[..bytes.len() - 10];
        let err = decode_record(truncated).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("trunc") || err.to_string().to_lowercase().contains("short"));

        let mut adapter = NexmonAdapter::from_bytes("t", SessionId(0), truncated.to_vec());
        assert!(adapter.next_frame().is_err());
        assert_eq!(adapter.health().frames_rejected, 1);
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut bytes = make_record(1, 6, 64, Some(-60));
        bytes[0] = 0xFF;
        assert!(decode_record(&bytes).is_err());
    }

    #[test]
    fn frames_from_bytes_helper() {
        let mut buf = make_record(10, 1, 64, Some(-50));
        buf.extend(make_record(20, 1, 64, Some(-51)));
        let frames = NexmonAdapter::frames_from_bytes("t", SessionId(1), &buf).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].timestamp_ns, 20);
    }
}
