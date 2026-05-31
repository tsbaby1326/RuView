//! Smoke tests for the cog-pose-estimation crate.
//!
//! These are deliberately tight — full inference integration tests
//! depend on a trained safetensors blob that doesn't live in-repo yet.

use cog_pose_estimation::{
    inference::{
        InferenceEngine, SyntheticInput, INPUT_SUBCARRIERS, INPUT_TIMESTEPS, OUTPUT_KEYPOINTS,
    },
    manifest::ManifestSpec,
};

#[test]
fn synthetic_window_has_correct_shape() {
    let syn = SyntheticInput;
    let window = syn.as_window();
    assert_eq!(window.data.len(), INPUT_SUBCARRIERS * INPUT_TIMESTEPS);
}

#[test]
fn engine_produces_finite_output_for_synthetic_input() {
    let engine = InferenceEngine::new().expect("engine init");
    let out = engine.infer(&SyntheticInput.as_window()).expect("infer");
    assert!(
        out.is_finite(),
        "synthetic input must produce finite output"
    );
    assert_eq!(out.keypoints.len(), OUTPUT_KEYPOINTS * 2);
}

#[test]
fn engine_rejects_wrong_shape_input() {
    let engine = InferenceEngine::new().expect("engine init");
    let bad = cog_pose_estimation::inference::CsiWindow {
        data: vec![0.0; 10],
    };
    assert!(engine.infer(&bad).is_err());
}

#[test]
fn real_weights_load_when_available() {
    use cog_pose_estimation::inference::InferenceEngine;
    let weights = std::path::Path::new("cog/artifacts/pose_v1.safetensors");
    if !weights.exists() {
        // Skip when running outside the repo (e.g. on a fresh appliance install).
        eprintln!("(skipping — cog/artifacts/pose_v1.safetensors not present in cwd)");
        return;
    }
    let engine = InferenceEngine::with_weights(Some(weights)).expect("load real weights");
    assert!(
        engine.backend().starts_with("candle-"),
        "expected real Candle backend, got {}",
        engine.backend()
    );
    let out = engine.infer(&SyntheticInput.as_window()).expect("infer");
    assert!(out.is_finite());
    // Real model emits the published validation PCK@50 as its self-reported
    // confidence — stub returns 0.0. This is the key assertion that proves
    // the cog isn't silently falling back to the stub.
    assert!(
        out.confidence > 0.0,
        "real model should emit non-zero confidence"
    );
}

#[test]
fn per_room_adapter_changes_inference_output() {
    // Build a minimal valid base + a non-trivial LoRA adapter in a tempdir, then verify
    // the calibration adapter (ADR-150 §3.5) is detected and actually alters the output.
    use candle_core::{DType, Device, Tensor};
    use std::collections::HashMap;

    let dev = Device::Cpu;
    let dir = std::env::temp_dir().join(format!("cogpose_adapter_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let base_p = dir.join("base.safetensors");
    let adapter_p = dir.join("room.adapter.safetensors");

    // --- base weights (random but finite) matching PoseNet's VarBuilder keys ---
    let mut w: HashMap<String, Tensor> = HashMap::new();
    let mut put = |k: &str, t: Tensor| {
        w.insert(k.to_string(), t);
    };
    put("enc.c1.weight", Tensor::randn(0f32, 0.1, (64, 56, 3), &dev).unwrap());
    put("enc.c1.bias", Tensor::zeros(64, DType::F32, &dev).unwrap());
    put("enc.c2.weight", Tensor::randn(0f32, 0.1, (128, 64, 3), &dev).unwrap());
    put("enc.c2.bias", Tensor::zeros(128, DType::F32, &dev).unwrap());
    put("enc.c3.weight", Tensor::randn(0f32, 0.1, (128, 128, 3), &dev).unwrap());
    put("enc.c3.bias", Tensor::zeros(128, DType::F32, &dev).unwrap());
    put("head.fc1.weight", Tensor::randn(0f32, 0.1, (256, 128), &dev).unwrap());
    put("head.fc1.bias", Tensor::zeros(256, DType::F32, &dev).unwrap());
    put("head.fc2.weight", Tensor::randn(0f32, 0.1, (34, 256), &dev).unwrap());
    put("head.fc2.bias", Tensor::zeros(34, DType::F32, &dev).unwrap());
    candle_core::safetensors::save(&w, &base_p).unwrap();

    // --- adapter: non-zero low-rank deltas on both head layers (scale baked into B) ---
    let r = 4usize;
    let mut ad: HashMap<String, Tensor> = HashMap::new();
    ad.insert("fc1.a".into(), Tensor::randn(0f32, 0.5, (128, r), &dev).unwrap());
    ad.insert("fc1.b".into(), Tensor::randn(0f32, 0.5, (r, 256), &dev).unwrap());
    ad.insert("fc2.a".into(), Tensor::randn(0f32, 0.5, (256, r), &dev).unwrap());
    ad.insert("fc2.b".into(), Tensor::randn(0f32, 0.5, (r, 34), &dev).unwrap());
    candle_core::safetensors::save(&ad, &adapter_p).unwrap();

    let base = InferenceEngine::with_weights(Some(&base_p)).expect("base load");
    let cal = InferenceEngine::with_weights_and_adapter(Some(&base_p), Some(&adapter_p))
        .expect("calibrated load");

    assert!(!base.is_calibrated(), "base must report uncalibrated");
    assert!(cal.is_calibrated(), "adapter engine must report calibrated");

    // Non-zero input — a zero window would zero the LoRA delta (x·A·B = 0).
    let win = cog_pose_estimation::inference::CsiWindow {
        data: (0..INPUT_SUBCARRIERS * INPUT_TIMESTEPS)
            .map(|i| ((i % 7) as f32 - 3.0) * 0.2)
            .collect(),
    };
    let a = base.infer(&win).expect("base infer");
    let b = cal.infer(&win).expect("calibrated infer");
    assert!(a.is_finite() && b.is_finite());

    let diff: f32 = a
        .keypoints
        .iter()
        .zip(&b.keypoints)
        .map(|(x, y)| (x - y).abs())
        .sum();
    assert!(
        diff > 1e-4,
        "per-room adapter must change the output (sum|Δ| = {diff})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn python_produced_adapter_loads_in_engine() {
    // Cross-language contract: an adapter fitted by `aether-arena/calibration/cog_calibrate.py`
    // (real LoRA on the cog conv+MLP head) must load + activate in this Rust engine.
    let base = std::path::Path::new("cog/artifacts/pose_v1.safetensors");
    if !base.exists() {
        eprintln!("(skipping — cog/artifacts/pose_v1.safetensors not present in cwd)");
        return;
    }
    let adapter = std::path::Path::new("tests/fixtures/sample_room.adapter.safetensors");
    assert!(adapter.exists(), "committed producer-generated adapter fixture is missing");

    let base_eng = InferenceEngine::with_weights(Some(base)).expect("base load");
    let cal_eng =
        InferenceEngine::with_weights_and_adapter(Some(base), Some(adapter)).expect("calibrated load");
    assert!(!base_eng.is_calibrated());
    assert!(cal_eng.is_calibrated(), "engine should report calibrated with the producer adapter");

    // Non-zero input so the LoRA delta is exercised.
    let win = cog_pose_estimation::inference::CsiWindow {
        data: (0..INPUT_SUBCARRIERS * INPUT_TIMESTEPS)
            .map(|i| ((i % 7) as f32 - 3.0) * 0.2)
            .collect(),
    };
    let a = base_eng.infer(&win).expect("base infer");
    let b = cal_eng.infer(&win).expect("calibrated infer");
    assert!(a.is_finite() && b.is_finite());
    let diff: f32 = a.keypoints.iter().zip(&b.keypoints).map(|(x, y)| (x - y).abs()).sum();
    assert!(diff > 1e-4, "python-produced adapter must change engine output (sum|Δ| = {diff})");
}

#[test]
fn manifest_roundtrips() {
    let spec = ManifestSpec::embedded("pose-estimation", "0.0.1");
    let s = serde_json::to_string(&spec).unwrap();
    let back: ManifestSpec = serde_json::from_str(&s).unwrap();
    assert_eq!(back.id, "pose-estimation");
    assert_eq!(back.version, "0.0.1");
}
