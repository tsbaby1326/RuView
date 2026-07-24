"""AETHER — contrastive CSI embeddings & re-identification (ADR-024, ADR-185 P1).

Self-supervised 128-dim L2-normalized embeddings for WiFi CSI: room
fingerprinting, person re-identification, and anomaly scoring, computed
entirely offline by the Rust core (no server, no network).

Included in the official ``wifi-densepose`` wheels. It is absent only from a
from-source build that did not enable the Rust ``aether`` feature; rebuild with
``maturin ... --features aether`` (or ``--features sota`` for all three P6
subsystems) in that case.

Quick start::

    from wifi_densepose.aether import AetherConfig, EmbeddingExtractor, cosine_similarity

    ext = EmbeddingExtractor(n_subcarriers=56, config=AetherConfig())
    a = ext.embed(window_a)          # list[float], length == config.d_proj (128)
    b = ext.embed(window_b)
    score = cosine_similarity(a, b)  # re-ID similarity in [-1, 1]
"""

from __future__ import annotations

from wifi_densepose import _native

# The AETHER symbols are compiled into `_native` only under the Rust
# `aether` feature, which the official wheels enable. They are absent only from
# a from-source build that omitted the feature — name the actual fix (rebuild
# with the feature), not a pip extra, which cannot add compiled code to an
# already-built wheel (ADR-185 §6 acceptance criterion).
if not hasattr(_native, "AetherConfig"):
    raise ImportError(
        "wifi_densepose.aether is not available in this build. The official "
        "wheels include it; if you built from source, rebuild with "
        "`maturin ... --features aether` (or `--features sota`)."
    )

AetherConfig = _native.AetherConfig
CsiAugmenter = _native.CsiAugmenter
EmbeddingExtractor = _native.EmbeddingExtractor
info_nce_loss = _native.info_nce_loss
cosine_similarity = _native.cosine_similarity

__all__ = [
    "AetherConfig",
    "CsiAugmenter",
    "EmbeddingExtractor",
    "info_nce_loss",
    "cosine_similarity",
]
