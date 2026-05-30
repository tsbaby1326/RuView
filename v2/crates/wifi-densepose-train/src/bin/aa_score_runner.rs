//! AetherArena ("AA") Deterministic Score Runner (ADR-149).
//!
//! The CI-runnable entry point behind the AA harness gate: it runs the **real**
//! `wifi-densepose-train::ruview_metrics` pose-acceptance harness against a
//! fixed, committed synthetic fixture (seed = 42) and emits:
//!   1. the pose metrics (PCK@0.2 all/torso, OKS, jitter, p95 error),
//!   2. the v0 `RuViewTier`-style pose verdict, and
//!   3. a cross-platform-stable SHA-256 **proof hash** of the quantised result.
//!
//! This is the `determinism_gate` substrate from ADR-149 §2.5: the same fixture
//! + same harness version must always produce the same hash. A PR that changes
//! the scoring maths moves the hash and fails the gate (the `expected_score.sha256`
//! must be regenerated and reviewed), so scorer drift can never land silently.
//!
//! Cross-platform portability (lesson from `calibration_proof_runner.rs`):
//!   PCK/OKS use `sqrt` (libm-sensitive: glibc/MSVC/Apple differ by ~1e-7). We
//!   never hash raw f32 — we quantise each metric to coarse fixed-point (1e-3 /
//!   1e-4) so a 1e-7 libm wobble is invisible while a real algorithm change
//!   (>1e-3) breaks the hash. No sort, no truncation.
//!
//! Usage:
//!   # verify against the committed expected hash (CI gate default):
//!   cargo run -p wifi-densepose-train --bin aa_score_runner --no-default-features
//!
//!   # emit the score as JSON (for the leaderboard ledger row):
//!   cargo run -p wifi-densepose-train --bin aa_score_runner --no-default-features -- --json
//!
//!   # regenerate the expected hash (after an intentional scorer change):
//!   cargo run -p wifi-densepose-train --bin aa_score_runner --no-default-features -- --generate-hash \
//!     > ../aether-arena/fixtures/expected_score.sha256

use std::env;
use std::process::ExitCode;

use ndarray::{Array1, Array2};
use sha2::{Digest, Sha256};
use wifi_densepose_train::ruview_metrics::{
    evaluate_joint_error, JointErrorResult, JointErrorThresholds,
};

/// Bump when the fixture or canonical hash form changes on purpose. Pinned into
/// the proof so a `harness_version` change forces a re-score (ADR-149 §2.4).
const AA_HARNESS_VERSION: u32 = 1;

/// Fixture size — fixed so the hash is stable.
const N_FRAMES: usize = 120;
const N_KPTS: usize = 17;

/// Deterministic, libm-free LCG (Numerical Recipes constants) → u32 → f32 in [0,1).
struct Lcg(u64);
impl Lcg {
    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 32) as u32
    }
    /// Uniform f32 in [0,1) at 1e-6 granularity — no float math in the generator.
    fn unit(&mut self) -> f32 {
        (self.next_u32() % 1_000_000) as f32 / 1_000_000.0
    }
}

/// Build the canonical fixture: ground-truth keypoints in [0.2,0.8] and
/// predictions = GT + a small, deterministic offset, so PCK/OKS land in a
/// stable mid-high band (not trivially 0 or 1). Identical on every platform.
fn build_fixture() -> (Vec<Array2<f32>>, Vec<Array2<f32>>, Vec<Array1<f32>>, Vec<f32>) {
    let mut rng = Lcg(42);
    let mut gt = Vec::with_capacity(N_FRAMES);
    let mut pred = Vec::with_capacity(N_FRAMES);
    let mut vis = Vec::with_capacity(N_FRAMES);
    let mut scale = Vec::with_capacity(N_FRAMES);

    for _ in 0..N_FRAMES {
        let mut g = Array2::<f32>::zeros((N_KPTS, 2));
        let mut p = Array2::<f32>::zeros((N_KPTS, 2));
        let mut v = Array1::<f32>::ones(N_KPTS);
        for k in 0..N_KPTS {
            let gx = 0.2 + 0.6 * rng.unit();
            let gy = 0.2 + 0.6 * rng.unit();
            // Deterministic prediction offset: small for most kpts, larger for a
            // few, so PCK is a believable fraction (~0.6-0.8) rather than 1.0.
            let ox = (rng.unit() - 0.5) * 0.06;
            let oy = (rng.unit() - 0.5) * 0.06;
            g[[k, 0]] = gx;
            g[[k, 1]] = gy;
            p[[k, 0]] = (gx + ox).clamp(0.0, 1.0);
            p[[k, 1]] = (gy + oy).clamp(0.0, 1.0);
            // Occlude ~10% deterministically.
            if rng.next_u32() % 10 == 0 {
                v[k] = 0.0;
            }
        }
        gt.push(g);
        pred.push(p);
        vis.push(v);
        scale.push(1.0);
    }
    (pred, gt, vis, scale)
}

/// Canonical, libm-stable byte form of the result for hashing.
/// Each metric → coarse fixed-point so ~1e-7 platform noise can't flip the hash.
fn canonical_bytes(r: &JointErrorResult) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"AA-SCORE-v0");
    b.extend_from_slice(&AA_HARNESS_VERSION.to_le_bytes());
    let q = |x: f32, scale: f32| -> u32 { (x.max(0.0) * scale).round() as u32 };
    b.extend_from_slice(&q(r.pck_all, 1e3).to_le_bytes());
    b.extend_from_slice(&q(r.pck_torso, 1e3).to_le_bytes());
    b.extend_from_slice(&q(r.oks, 1e3).to_le_bytes());
    b.extend_from_slice(&q(r.jitter_rms_m, 1e4).to_le_bytes());
    b.extend_from_slice(&q(r.max_error_p95_m, 1e4).to_le_bytes());
    b.push(r.passes as u8);
    b
}

fn hash_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|x| format!("{x:02x}")).collect()
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let mode_json = args.iter().any(|a| a == "--json");
    let mode_gen = args.iter().any(|a| a == "--generate-hash");

    let (pred, gt, vis, scale) = build_fixture();
    let result = evaluate_joint_error(&pred, &gt, &vis, &scale, &JointErrorThresholds::default());
    let proof = hash_hex(&canonical_bytes(&result));

    if mode_gen {
        // Emit just the hash (stdout) for redirection into expected_score.sha256.
        println!("{proof}");
        return ExitCode::SUCCESS;
    }

    if mode_json {
        // One leaderboard-ledger-shaped row (ADR-149 §2.2).
        println!(
            "{{\"category\":\"pose\",\"harness_version\":{},\"pck_all\":{:.4},\"pck_torso\":{:.4},\"oks\":{:.4},\"jitter_rms_m\":{:.5},\"max_error_p95_m\":{:.5},\"pose_passes\":{},\"proof_sha256\":\"{}\"}}",
            AA_HARNESS_VERSION,
            result.pck_all, result.pck_torso, result.oks,
            result.jitter_rms_m, result.max_error_p95_m, result.passes, proof
        );
        return ExitCode::SUCCESS;
    }

    // Default: verify against the committed expected hash (CI gate).
    let expected_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../aether-arena/fixtures/expected_score.sha256");
    let expected = std::fs::read_to_string(expected_path)
        .ok()
        .map(|s| s.trim().to_string());

    println!("AA pose score: PCK_all={:.4} PCK_torso={:.4} OKS={:.4} jitter={:.5}m p95={:.5}m passes={}",
        result.pck_all, result.pck_torso, result.oks, result.jitter_rms_m, result.max_error_p95_m, result.passes);
    println!("AA proof sha256: {proof}");

    match expected {
        Some(exp) if exp == proof => {
            println!("VERDICT: PASS (determinism hash matches expected)");
            ExitCode::SUCCESS
        }
        Some(exp) => {
            eprintln!("VERDICT: FAIL — scorer drift detected.\n  expected: {exp}\n  actual:   {proof}");
            eprintln!("If this change to the scoring maths is intentional, regenerate with --generate-hash and review the diff.");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("VERDICT: NO-EXPECTED-HASH — {expected_path} missing. Generate with --generate-hash.");
            ExitCode::FAILURE
        }
    }
}
