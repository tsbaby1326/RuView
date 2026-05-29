//! Criterion benchmark for the RuView streaming-engine hot path.
//!
//! The live system runs at 20 Hz → a **50 ms** wall-clock budget per cycle.
//! This measures one full [`StreamingEngine::process_cycle`] (fuse + quality
//! scoring + calibration provenance + privacy gate + WorldGraph semantic node)
//! for a 4-node / 56-subcarrier mesh — the realistic ESP32-S3 HT20 case.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use wifi_densepose_bfld::PrivacyMode;
use wifi_densepose_engine::StreamingEngine;
use wifi_densepose_geo::types::GeoRegistration;
use wifi_densepose_signal::hardware_norm::{CanonicalCsiFrame, HardwareType};
use wifi_densepose_signal::ruvsense::fusion_quality::CalibrationId;
use wifi_densepose_signal::ruvsense::MultiBandCsiFrame;

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

fn bench_cycle(c: &mut Criterion) {
    let frames: Vec<MultiBandCsiFrame> =
        (0..4).map(|i| node_frame(i, 1000 + u64::from(i), 56)).collect();

    c.bench_function("process_cycle_4nodes_56sc", |b| {
        b.iter_batched(
            || {
                let mut e =
                    StreamingEngine::new(PrivacyMode::PrivateHome, 1, GeoRegistration::default());
                let room = e.add_room("living_room", "Living Room");
                e.add_sensor("esp32-com9", room);
                (e, room)
            },
            |(mut e, room)| {
                e.process_cycle(&frames, CalibrationId(1), room, 0).unwrap()
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_cycle);
criterion_main!(benches);
