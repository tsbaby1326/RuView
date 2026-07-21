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
