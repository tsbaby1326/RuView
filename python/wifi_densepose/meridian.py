"""MERIDIAN — cross-environment domain generalization (ADR-027, ADR-185 P2).

Hardware-invariant CSI normalization, geometry-conditioned deployment,
few-shot room adaptation, and cross-domain evaluation — the tch-free
inference/adaptation path of Project MERIDIAN, computed by the Rust core.

Available **only** when the wheel was built with the ``[meridian]`` extra::

    pip install wifi-densepose[meridian]

Quick start::

    from wifi_densepose.meridian import HardwareNormalizer, HardwareType

    norm = HardwareNormalizer()                      # canonical 56 subcarriers
    hw = HardwareType.detect(64)                     # -> HardwareType.Esp32S3
    frame = norm.normalize(amplitude, phase, hw)     # -> CanonicalCsiFrame
    print(len(frame.amplitude), frame.hardware_type)

Note (honest scope, ADR-185 §3.3): the ADR's ``RapidAdaptation.calibrate``
/ ``AdaptationResult.converged`` do not exist in the Rust core — use
``push_frame(...)`` then ``adapt()``; the result exposes ``final_loss``,
``frames_used``, ``adaptation_epochs``. Training-time types
(DomainFactorizer, GradientReversalLayer, VirtualDomainAugmentor) are
out of scope for P6 (they need the deferred libtorch training tier).
"""

from __future__ import annotations

from wifi_densepose import _native

# MERIDIAN symbols are compiled into `_native` only under the Rust
# `meridian` feature; absent in a base wheel (ADR-185 §6 acceptance).
if not hasattr(_native, "HardwareNormalizer"):
    raise ImportError(
        "wifi_densepose.meridian is not available in this wheel. "
        "It requires the 'meridian' extra:  pip install wifi-densepose[meridian]"
    )

HardwareType = _native.HardwareType
CanonicalCsiFrame = _native.CanonicalCsiFrame
HardwareNormalizer = _native.HardwareNormalizer
MeridianGeometryConfig = _native.MeridianGeometryConfig
GeometryEncoder = _native.GeometryEncoder
RapidAdaptation = _native.RapidAdaptation
AdaptationResult = _native.AdaptationResult
CrossDomainEvaluator = _native.CrossDomainEvaluator
mpjpe = _native.mpjpe

__all__ = [
    "HardwareType",
    "CanonicalCsiFrame",
    "HardwareNormalizer",
    "MeridianGeometryConfig",
    "GeometryEncoder",
    "RapidAdaptation",
    "AdaptationResult",
    "CrossDomainEvaluator",
    "mpjpe",
]
