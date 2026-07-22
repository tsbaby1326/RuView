//! ADR-185 §4.1 — AETHER bit-for-bit parity: native-Rust reference half.
//!
//! Produces the golden 128-dim embedding by calling the canonical
//! `wifi-densepose-aether::embedding` code DIRECTLY (no PyO3),
//! for the committed `tests/golden/aether_input.json` fixture, and locks
//! its SHA-256 into `tests/golden/aether_embedding.sha256`.
//!
//! The pytest half (`tests/test_aether.py`) independently runs the same
//! fixture through the Python binding and asserts the identical hash —
//! together they prove the binding is byte-identical to native Rust.
//!
//! Regeneration (only when the Rust subsystem intentionally changes):
//! delete `tests/golden/aether_embedding.sha256` and re-run
//! `cargo test --features aether`.
#![cfg(feature = "aether")]

use std::fs;
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use wifi_densepose_aether::embedding::{EmbeddingConfig, EmbeddingExtractor};
use wifi_densepose_aether::graph_transformer::TransformerConfig;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
}

fn load_input() -> Vec<Vec<f32>> {
    let raw = fs::read_to_string(golden_dir().join("aether_input.json"))
        .expect("read aether_input.json fixture");
    let rows: Vec<Vec<f64>> = serde_json::from_str(&raw).expect("parse aether_input.json");
    rows.into_iter()
        .map(|row| row.into_iter().map(|x| x as f32).collect())
        .collect()
}

/// Build the extractor identically to the Python binding's default
/// construction: `AetherConfig()` + `EmbeddingExtractor(n_subcarriers=56, cfg)`.
fn embed_native(input: &[Vec<f32>]) -> Vec<f32> {
    let e_config = EmbeddingConfig {
        d_model: 64,
        d_proj: 128,
        temperature: 0.07,
        normalize: true,
    };
    let t_config = TransformerConfig {
        n_subcarriers: 56,
        n_keypoints: 17,
        d_model: 64,
        n_heads: 4,
        n_gnn_layers: 2,
    };
    let mut ext = EmbeddingExtractor::new(t_config, e_config);
    ext.extract(input)
}

fn sha256_le(embedding: &[f32]) -> String {
    let mut hasher = Sha256::new();
    for &x in embedding {
        hasher.update(x.to_le_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn native_embedding_is_128_dim_unit_norm() {
    let emb = embed_native(&load_input());
    assert_eq!(emb.len(), 128, "AETHER embedding must be 128-dim");
    let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-4,
        "embedding must be L2-normalized, got norm={norm}"
    );
}

#[test]
fn native_embedding_matches_committed_golden() {
    let emb = embed_native(&load_input());
    let got = sha256_le(&emb);
    let path = golden_dir().join("aether_embedding.sha256");
    match fs::read_to_string(&path) {
        Ok(expected) => assert_eq!(
            got,
            expected.trim(),
            "native AETHER embedding hash drifted from committed golden \
             (intentional? delete the .sha256 and regenerate)"
        ),
        Err(_) => {
            fs::write(&path, &got).expect("write golden sha256");
            panic!("no committed golden found; wrote {got}. Re-run to verify parity.");
        }
    }
}
