"""AETHER — contrastive CSI embeddings & re-identification (ADR-024, ADR-185 P1).

Self-supervised 128-dim L2-normalized embeddings for WiFi CSI: room
fingerprinting, person re-identification, and anomaly scoring, computed
entirely offline by the Rust core (no server, no network).

Available **only** when the wheel was built with the ``[aether]`` extra::

    pip install wifi-densepose[aether]

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
# `aether` feature. In a base (`pip install wifi-densepose`) wheel they
# are absent — surface a clear, actionable error naming the extra
# (ADR-185 §6 acceptance criterion).
if not hasattr(_native, "AetherConfig"):
    raise ImportError(
        "wifi_densepose.aether is not available in this wheel. "
        "It requires the 'aether' extra:  pip install wifi-densepose[aether]"
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
