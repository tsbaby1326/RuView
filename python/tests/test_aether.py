"""ADR-185 P1 — AETHER binding tests, incl. the §4.1 bit-for-bit parity gate.

The parity test packs the binding's embedding to little-endian f32 bytes
and asserts its SHA-256 equals the committed golden produced by the
native-Rust reference (`tests/aether_parity.rs`). A mismatch is a
release blocker, not a warning.
"""

from __future__ import annotations

import hashlib
import json
import math
import struct
import tempfile
from pathlib import Path

import pytest

from wifi_densepose import aether

GOLDEN = Path(__file__).parent / "golden"


def load_input() -> list[list[float]]:
    return json.loads((GOLDEN / "aether_input.json").read_text())


def build_extractor() -> aether.EmbeddingExtractor:
    # Must match the native-Rust reference construction exactly.
    cfg = aether.AetherConfig(d_model=64, d_proj=128, temperature=0.07, normalize=True)
    return aether.EmbeddingExtractor(n_subcarriers=56, config=cfg)


def _formula_weights(n: int) -> list[float]:
    # Byte-identical to aether_weights_parity.rs (k/65536 is exact in f32+f64).
    return [((i * 1103515245 + 12345) % 65536) / 65536.0 - 0.5 for i in range(n)]


def _write_weight_file(path: Path, weights: list[float]) -> None:
    # AETHER weight format: b"AETHERW1" + u32 count + LE f32 payload.
    with open(path, "wb") as f:
        f.write(b"AETHERW1")
        f.write(struct.pack("<I", len(weights)))
        f.write(b"".join(struct.pack("<f", w) for w in weights))


def test_config_roundtrips_fields() -> None:
    cfg = aether.AetherConfig(d_model=64, d_proj=128, temperature=0.07, normalize=True)
    assert cfg.d_model == 64
    assert cfg.d_proj == 128
    assert abs(cfg.temperature - 0.07) < 1e-6
    assert cfg.normalize is True


def test_embedding_shape_and_unit_norm() -> None:
    emb = build_extractor().embed(load_input())
    assert len(emb) == 128
    norm = math.sqrt(sum(x * x for x in emb))
    assert abs(norm - 1.0) < 1e-4, f"expected unit-norm embedding, got {norm}"


def test_bit_for_bit_parity_with_native_rust() -> None:
    """The release-blocking §4.1 gate: binding output == native Rust, byte-for-byte."""
    emb = build_extractor().embed(load_input())
    packed = b"".join(struct.pack("<f", x) for x in emb)
    got = hashlib.sha256(packed).hexdigest()
    expected = (GOLDEN / "aether_embedding.sha256").read_text().strip()
    assert got == expected, (
        "Python binding embedding diverged from the native-Rust golden "
        f"({got} != {expected}) — PyO3 marshalling is not byte-identical."
    )


def test_embedding_is_deterministic() -> None:
    ext = build_extractor()
    inp = load_input()
    assert ext.embed(inp) == ext.embed(inp)


def test_cosine_similarity_self_is_one() -> None:
    v = [0.1 * i - 0.5 for i in range(32)]
    assert abs(aether.cosine_similarity(v, v) - 1.0) < 1e-5


def test_cosine_similarity_orthogonal_is_zero() -> None:
    a = [1.0, 0.0, 0.0, 0.0]
    b = [0.0, 1.0, 0.0, 0.0]
    assert abs(aether.cosine_similarity(a, b)) < 1e-6


def test_info_nce_loss_identical_batch_is_log_n() -> None:
    # Identical embeddings → all similarities equal → loss == ln(N).
    emb = [[1.0, 0.0, 0.0]] * 4
    loss = aether.info_nce_loss(emb, emb, 0.07)
    assert abs(loss - math.log(4)) < 0.1


def test_augment_pair_preserves_shape_and_differs() -> None:
    window = load_input()
    view_a, view_b = aether.CsiAugmenter().augment_pair(window, seed=42)
    assert len(view_a) == len(window)
    assert len(view_b) == len(window)
    assert len(view_a[0]) == len(window[0])
    differs = any(
        abs(x - y) > 1e-6
        for ra, rb in zip(view_a, view_b)
        for x, y in zip(ra, rb)
    )
    assert differs, "augment_pair should return two distinct views"


def test_base_wheel_import_error_message() -> None:
    # This wheel HAS the extra, so the import succeeds; assert the guard
    # message is present in source so the base-wheel path stays honest.
    src = (Path(aether.__file__)).read_text()
    assert "pip install wifi-densepose[aether]" in src


# ─── Weight loading (ADR-185 §13.a) ──────────────────────────────────

def test_load_weights_is_used_and_matches_native_golden() -> None:
    ext = build_extractor()
    baseline = ext.embed(load_input())  # random Xavier init

    weights = _formula_weights(ext.param_count)
    with tempfile.TemporaryDirectory() as d:
        wpath = Path(d) / "weights.bin"
        _write_weight_file(wpath, weights)
        ext.load_weights(str(wpath))

    loaded = ext.embed(load_input())

    # (1) The loaded weights actually take effect (not a silent no-op).
    assert any(abs(a - b) > 1e-6 for a, b in zip(baseline, loaded)), (
        "load_weights had no effect — embedding still equals the random-init baseline"
    )
    # (2) Bit-identical to the native-Rust reference that loaded the same weights.
    packed = b"".join(struct.pack("<f", x) for x in loaded)
    got = hashlib.sha256(packed).hexdigest()
    expected = (GOLDEN / "aether_loaded_embedding.sha256").read_text().strip()
    assert got == expected, (
        f"binding loaded-weights embedding diverged from native golden ({got} != {expected})"
    )


def test_save_then_load_weights_round_trips() -> None:
    ext = build_extractor()
    inp = load_input()
    with tempfile.TemporaryDirectory() as d:
        wpath = Path(d) / "roundtrip.bin"
        ext.save_weights(str(wpath))          # serialize current (random) weights
        emb_before = ext.embed(inp)
        ext2 = build_extractor()
        ext2.load_weights(str(wpath))         # load them into a fresh extractor
    assert ext2.embed(inp) == emb_before


def test_load_weights_rejects_bad_magic() -> None:
    ext = build_extractor()
    with tempfile.TemporaryDirectory() as d:
        wpath = Path(d) / "bad.bin"
        wpath.write_bytes(b"NOTAETHER" + b"\x00" * 8)
        with pytest.raises(ValueError):
            ext.load_weights(str(wpath))


def test_load_weights_rejects_wrong_param_count() -> None:
    ext = build_extractor()
    with tempfile.TemporaryDirectory() as d:
        wpath = Path(d) / "short.bin"
        _write_weight_file(wpath, [0.1, 0.2, 0.3])  # far too few params
        with pytest.raises(ValueError):
            ext.load_weights(str(wpath))
