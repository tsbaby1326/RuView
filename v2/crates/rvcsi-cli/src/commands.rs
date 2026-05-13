//! Implementations of the `rvcsi` subcommands (ADR-095 FR7).
//!
//! Each command writes to a caller-supplied `&mut dyn Write` so the bodies can
//! be unit-tested against an in-memory buffer.

use std::io::Write;

use anyhow::{Context, Result};

use rvcsi_adapter_file::{read_all, CaptureHeader, FileRecorder, FileReplayAdapter};
use rvcsi_adapter_nexmon::NexmonAdapter;
use rvcsi_core::{
    validate_frame, AdapterKind, AdapterProfile, CsiFrame, CsiSource, SessionId, SourceId,
    ValidationPolicy,
};
use rvcsi_runtime as runtime;

/// `rvcsi record --in <nexmon.bin> --out <cap.rvcsi>` — transcode a buffer of
/// "rvCSI Nexmon records" (the napi-c shim format) into a `.rvcsi` capture file,
/// validating each frame on the way in. This gives the CLI a way to produce
/// `.rvcsi` files without a live radio (which needs the not-yet-shipped daemon).
pub fn record_from_nexmon(
    out: &mut dyn Write,
    nexmon_path: &str,
    out_path: &str,
    source_id: &str,
    session_id: u64,
) -> Result<()> {
    let bytes = std::fs::read(nexmon_path).with_context(|| format!("reading {nexmon_path}"))?;
    let mut src = NexmonAdapter::from_bytes(SourceId::from(source_id), SessionId(session_id), bytes);
    let profile = AdapterProfile::offline(AdapterKind::Nexmon);
    let policy = ValidationPolicy::default();
    let header = CaptureHeader::new(SessionId(session_id), SourceId::from(source_id), profile.clone());
    let mut rec = FileRecorder::create(out_path, &header).with_context(|| format!("creating {out_path}"))?;
    let (mut written, mut skipped, mut prev_ts) = (0u64, 0u64, None);
    loop {
        match src.next_frame() {
            Ok(None) => break,
            Ok(Some(mut f)) => {
                let ts = f.timestamp_ns;
                match validate_frame(&mut f, &profile, &policy, prev_ts) {
                    Ok(()) if f.is_exposable() => {
                        prev_ts = Some(ts);
                        rec.write_frame(&f)?;
                        written += 1;
                    }
                    _ => skipped += 1,
                }
            }
            Err(e) => {
                writeln!(out, "warning: stopped at a malformed Nexmon record: {e}")?;
                break;
            }
        }
    }
    rec.finish()?;
    writeln!(out, "recorded {written} frame(s) to {out_path} ({skipped} dropped by validation)")?;
    Ok(())
}

/// `rvcsi inspect <path>` — print a summary of a `.rvcsi` capture file.
pub fn inspect(out: &mut dyn Write, path: &str, json: bool) -> Result<()> {
    let summary = runtime::summarize_capture(path).with_context(|| format!("inspecting {path}"))?;
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(&summary)?)?;
        return Ok(());
    }
    writeln!(out, "capture        : {path}")?;
    writeln!(out, "  version      : {}", summary.capture_version)?;
    writeln!(out, "  session      : {}", summary.session_id)?;
    writeln!(out, "  source       : {}", summary.source_id)?;
    writeln!(out, "  adapter      : {}", summary.adapter_kind)?;
    writeln!(out, "  frames       : {}", summary.frame_count)?;
    writeln!(
        out,
        "  time span    : {} .. {} ns ({} ns)",
        summary.first_timestamp_ns,
        summary.last_timestamp_ns,
        summary.last_timestamp_ns.saturating_sub(summary.first_timestamp_ns)
    )?;
    writeln!(out, "  channels     : {:?}", summary.channels)?;
    writeln!(out, "  subcarriers  : {:?}", summary.subcarrier_counts)?;
    writeln!(out, "  mean quality : {:.3}", summary.mean_quality)?;
    let b = summary.validation_breakdown;
    writeln!(
        out,
        "  validation   : accepted={} degraded={} recovered={} rejected={} pending={}",
        b.accepted, b.degraded, b.recovered, b.rejected, b.pending
    )?;
    writeln!(out, "  calibration  : {}", summary.calibration_version.as_deref().unwrap_or("(none)"))?;
    Ok(())
}

/// `rvcsi replay <path>` / `rvcsi stream --in <path> --format json` — emit one
/// line per frame. With `json`, the full `CsiFrame` JSON; otherwise a compact
/// `frame_id ts ch rssi quality validation` line. `limit` caps the count
/// (`None` = all). `speed` is accepted but not enforced here (the daemon paces
/// real-time replay); a non-1.0 value is noted on stderr by the caller.
pub fn replay(out: &mut dyn Write, path: &str, json: bool, limit: Option<usize>) -> Result<()> {
    let mut adapter = FileReplayAdapter::open(path).with_context(|| format!("opening {path}"))?;
    let mut n = 0usize;
    while let Some(frame) = adapter.next_frame()? {
        if json {
            writeln!(out, "{}", serde_json::to_string(&frame)?)?;
        } else {
            writeln!(
                out,
                "{:>8} {:>16} ch{:<3} rssi={:>5} q={:.3} {:?}",
                frame.frame_id.value(),
                frame.timestamp_ns,
                frame.channel,
                frame.rssi_dbm.map(|r| r.to_string()).unwrap_or_else(|| "-".into()),
                frame.quality_score,
                frame.validation,
            )?;
        }
        n += 1;
        if let Some(lim) = limit {
            if n >= lim {
                break;
            }
        }
    }
    if !json {
        writeln!(out, "-- {n} frame(s)")?;
    }
    Ok(())
}

/// `rvcsi events <path>` — replay the capture through DSP + the event pipeline
/// and print the emitted events (compact, or full JSON with `json`).
pub fn events(out: &mut dyn Write, path: &str, json: bool) -> Result<()> {
    let evs = runtime::events_from_capture(path).with_context(|| format!("processing {path}"))?;
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(&evs)?)?;
        return Ok(());
    }
    for e in &evs {
        writeln!(
            out,
            "{:>16} ns  {:<22} conf={:.3}  evidence={:?}{}",
            e.timestamp_ns,
            e.kind.slug(),
            e.confidence,
            e.evidence_window_ids.iter().map(|w| w.value()).collect::<Vec<_>>(),
            e.calibration_version.as_deref().map(|c| format!("  calib={c}")).unwrap_or_default(),
        )?;
    }
    writeln!(out, "-- {} event(s)", evs.len())?;
    Ok(())
}

/// `rvcsi health --source <slug> [--target <path>]` — open the source, drain it,
/// and print the final `SourceHealth` as JSON. File and Nexmon sources work
/// offline; live radios are not available in this build.
pub fn health(out: &mut dyn Write, source: &str, target: Option<&str>) -> Result<()> {
    let h = match source {
        "file" | "replay" => {
            let path = target.context("`--target <path>` is required for the file source")?;
            let mut a = FileReplayAdapter::open(path)?;
            while a.next_frame()?.is_some() {}
            a.health()
        }
        "nexmon" => {
            let path = target.context("`--target <path>` is required for the nexmon source")?;
            let bytes = std::fs::read(path)?;
            let mut a = NexmonAdapter::from_bytes(SourceId::from("nexmon"), SessionId(0), bytes);
            // pull until exhausted or a malformed record stops us
            while let Ok(Some(_)) = a.next_frame() {}
            a.health()
        }
        "esp32" | "intel" | "atheros" => {
            anyhow::bail!("live capture for source `{source}` is not available in this build; use the `rvcsi-daemon` (not yet shipped) or replay a `.rvcsi` capture");
        }
        other => anyhow::bail!("unknown source `{other}` (expected: file, replay, nexmon, esp32, intel, atheros)"),
    };
    writeln!(out, "{}", serde_json::to_string_pretty(&h)?)?;
    Ok(())
}

/// `rvcsi export ruvector --in <capture> --out <jsonl>` — window the capture and
/// store each window's embedding into a JSONL RF-memory file.
pub fn export_ruvector(out: &mut dyn Write, capture: &str, out_jsonl: &str) -> Result<()> {
    let stored = runtime::export_capture_to_rf_memory(capture, out_jsonl)
        .with_context(|| format!("exporting {capture} -> {out_jsonl}"))?;
    writeln!(out, "stored {stored} window embedding(s) to {out_jsonl}")?;
    Ok(())
}

/// `rvcsi calibrate --in <capture> [--out <baseline.json>]` — a v0 calibration:
/// learn the per-subcarrier mean amplitude (the "baseline") over all exposable
/// frames in a capture and emit it as JSON. Real, versioned, room-scoped
/// calibration (ADR-095 D14) lands with the daemon.
pub fn calibrate(out: &mut dyn Write, capture: &str, out_path: Option<&str>) -> Result<()> {
    let (header, frames) = read_all(capture).with_context(|| format!("reading {capture}"))?;
    let exposable: Vec<&CsiFrame> = frames.iter().filter(|f| f.is_exposable()).collect();
    if exposable.is_empty() {
        anyhow::bail!("no exposable frames in {capture} — cannot calibrate");
    }
    let n = exposable[0].subcarrier_count as usize;
    let mut acc = vec![0.0f64; n];
    let mut count = 0usize;
    for f in &exposable {
        if f.subcarrier_count as usize != n {
            continue;
        }
        for (a, v) in acc.iter_mut().zip(f.amplitude.iter()) {
            *a += *v as f64;
        }
        count += 1;
    }
    let baseline: Vec<f32> = acc.iter().map(|a| (*a / count.max(1) as f64) as f32).collect();
    #[derive(serde::Serialize)]
    struct Baseline<'a> {
        source_id: &'a str,
        session_id: u64,
        version: String,
        subcarrier_count: usize,
        frames_used: usize,
        baseline_amplitude: Vec<f32>,
    }
    let payload = Baseline {
        source_id: header.source_id.as_str(),
        session_id: header.session_id.value(),
        version: format!("{}@auto-{count}", header.source_id.as_str()),
        subcarrier_count: n,
        frames_used: count,
        baseline_amplitude: baseline,
    };
    let json = serde_json::to_string_pretty(&payload)?;
    if let Some(p) = out_path {
        std::fs::write(p, &json)?;
        writeln!(out, "wrote baseline ({n} subcarriers, {count} frames) to {p}")?;
    } else {
        writeln!(out, "{json}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rvcsi_adapter_nexmon::{encode_record, NexmonRecord};
    use rvcsi_core::{FrameId, ValidationStatus};

    fn write_capture(path: &std::path::Path, n: usize) {
        let header = CaptureHeader::new(
            SessionId(2),
            SourceId::from("cli-it"),
            AdapterProfile::offline(AdapterKind::File),
        );
        let mut rec = FileRecorder::create(path, &header).unwrap();
        for k in 0..n {
            let amp_scale = if (k / 8) % 2 == 0 { 0.0 } else { 1.5 };
            let i: Vec<f32> = (0..32).map(|s| 1.0 + amp_scale * (((k + s) % 5) as f32 - 2.0)).collect();
            let q: Vec<f32> = (0..32).map(|_| 0.5).collect();
            let mut f = CsiFrame::from_iq(
                FrameId(k as u64),
                SessionId(2),
                SourceId::from("cli-it"),
                AdapterKind::File,
                1_000 + k as u64 * 50_000_000,
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

    fn run<F: FnOnce(&mut Vec<u8>) -> Result<()>>(f: F) -> String {
        let mut buf = Vec::new();
        f(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn inspect_human_and_json() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_capture(tmp.path(), 12);
        let p = tmp.path().to_str().unwrap();
        let human = run(|o| inspect(o, p, false));
        assert!(human.contains("frames       : 12"));
        assert!(human.contains("channels     : [6]"));
        let json = run(|o| inspect(o, p, true));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["frame_count"], 12);
    }

    #[test]
    fn replay_compact_and_json_and_limit() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_capture(tmp.path(), 5);
        let p = tmp.path().to_str().unwrap();
        let compact = run(|o| replay(o, p, false, None));
        assert!(compact.contains("-- 5 frame(s)"));
        let json = run(|o| replay(o, p, true, Some(3)));
        assert_eq!(json.lines().count(), 3);
        for line in json.lines() {
            let _: CsiFrame = serde_json::from_str(line).unwrap();
        }
    }

    #[test]
    fn events_command_emits_something() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_capture(tmp.path(), 64);
        let p = tmp.path().to_str().unwrap();
        let out = run(|o| events(o, p, false));
        assert!(out.contains("event(s)"));
        let json = run(|o| events(o, p, true));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.is_array());
    }

    #[test]
    fn health_file_source() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_capture(tmp.path(), 7);
        let p = tmp.path().to_str().unwrap();
        let out = run(|o| health(o, "file", Some(p)));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["frames_delivered"], 7);
        assert_eq!(v["connected"], false);
        // unknown / live sources error cleanly
        let mut buf = Vec::new();
        assert!(health(&mut buf, "esp32", Some(p)).is_err());
        assert!(health(&mut buf, "bogus", None).is_err());
        assert!(health(&mut buf, "file", None).is_err()); // missing --target
    }

    #[test]
    fn export_and_calibrate() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_capture(tmp.path(), 64);
        let p = tmp.path().to_str().unwrap();
        let out_jsonl = tempfile::NamedTempFile::new().unwrap();
        let out = run(|o| export_ruvector(o, p, out_jsonl.path().to_str().unwrap()));
        assert!(out.contains("stored "));
        // calibrate to stdout
        let calib = run(|o| calibrate(o, p, None));
        let v: serde_json::Value = serde_json::from_str(&calib).unwrap();
        assert_eq!(v["subcarrier_count"], 32);
        assert!(v["baseline_amplitude"].as_array().unwrap().len() == 32);
        // calibrate to file
        let baseline_file = tempfile::NamedTempFile::new().unwrap();
        let out2 = run(|o| calibrate(o, p, Some(baseline_file.path().to_str().unwrap())));
        assert!(out2.contains("wrote baseline"));
        let written = std::fs::read_to_string(baseline_file.path()).unwrap();
        assert!(written.contains("baseline_amplitude"));
    }

    #[test]
    fn record_from_nexmon_then_inspect_and_replay() {
        // build a small Nexmon record dump (64-subcarrier, the default profile)
        let mut dump = Vec::new();
        for k in 0..6u64 {
            let rec = NexmonRecord {
                subcarrier_count: 64,
                channel: 36,
                bandwidth_mhz: 80,
                rssi_dbm: Some(-60 - k as i16),
                noise_floor_dbm: Some(-92),
                timestamp_ns: 1_000 + k * 50_000_000,
                i_values: (0..64).map(|s| (s as f32 % 3.0) - 1.0).collect(),
                q_values: (0..64).map(|s| (s as f32 % 5.0) * 0.1).collect(),
            };
            dump.extend(encode_record(&rec).unwrap());
        }
        let dump_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(dump_file.path(), &dump).unwrap();
        let cap_file = tempfile::NamedTempFile::new().unwrap();

        let out = run(|o| {
            record_from_nexmon(
                o,
                dump_file.path().to_str().unwrap(),
                cap_file.path().to_str().unwrap(),
                "nexmon-rec",
                3,
            )
        });
        assert!(out.contains("recorded 6 frame(s)"), "{out}");

        // the produced capture is a real .rvcsi the other commands can read
        let summary = run(|o| inspect(o, cap_file.path().to_str().unwrap(), false));
        assert!(summary.contains("frames       : 6"));
        assert!(summary.contains("source       : nexmon-rec"));
        let replayed = run(|o| replay(o, cap_file.path().to_str().unwrap(), false, None));
        assert!(replayed.contains("-- 6 frame(s)"));
    }

    #[test]
    fn errors_on_missing_capture() {
        let mut buf = Vec::new();
        assert!(inspect(&mut buf, "/no/such/file.rvcsi", false).is_err());
        assert!(replay(&mut buf, "/no/such/file.rvcsi", false, None).is_err());
        assert!(events(&mut buf, "/no/such/file.rvcsi", false).is_err());
        assert!(calibrate(&mut buf, "/no/such/file.rvcsi", None).is_err());
        assert!(record_from_nexmon(&mut buf, "/no/x.bin", "/tmp/y.rvcsi", "s", 0).is_err());
    }
}
