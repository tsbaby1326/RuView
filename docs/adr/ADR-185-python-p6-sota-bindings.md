# ADR-185: Python P6 SOTA bindings — AETHER, MERIDIAN, and MAT via PyO3 extras

| Field | Value |
|-------|-------|
| **Status** | Proposed |
| **Date** | 2026-07-21 |
| **Deciders** | ruv |
| **Codename** | **PIP-TRINITY** — three SOTA subsystems join the `wifi_densepose` wheel |
| **Relates to** | [ADR-117](ADR-117-pip-wifi-densepose-modernization.md) (PIP-PHOENIX — the PyO3 wheel this extends), [ADR-024](ADR-024-contrastive-csi-embedding-model.md) (AETHER contrastive embeddings), [ADR-027](ADR-027-cross-environment-domain-generalization.md) (MERIDIAN domain generalization), [ADR-152](ADR-152-wifi-pose-sota-2026.md) (WiFlow-STD ~96% PCK@20 SOTA bar) |
| **Tracking issue** | TBD — file under RuView issue tracker |

---

## 1. Context

### 1.1 Where ADR-117 stopped

ADR-117 (PIP-PHOENIX) shipped the `wifi-densepose` v2.x PyPI wheel as a PyO3 +
maturin compiled extension (`wifi_densepose._native`) with a pure-Python facade.
The bound surface today (`python/src/bindings/*.rs`, `python/src/lib.rs`):

| Bound today | Crate | Kind |
|---|---|---|
| `CsiFrame`, `Keypoint`, `KeypointType`, `BoundingBox`, `PersonPose`, `PoseEstimate` | `wifi-densepose-core` | P2 core types |
| 4-stage vitals (`BreathingExtractor`, `HeartRateExtractor`, `VitalEstimate`, `VitalReading`, `VitalStatus`) | `wifi-densepose-vitals` | P3 DSP |
| `BfldFrame`, `BfldReport`, `BfldKind` + `PrivacyClass` gate | `wifi-densepose-bfld` | P3.5 / ADR-118 |
| `SensingClient` (WS), `RuViewMqttClient` (MQTT), HA helpers | pure-Python `wifi_densepose.client` | P4 `[client]` extra |

ADR-117's own phase ledger (§6, "P6+ — Deferred") explicitly parked three
higher-value subsystems as post-v2.0.0 work:

> - [ ] `wifi-densepose-nn` bindings … · `wifi-densepose-ruvector` bindings …
> - [ ] MQTT/Matter integration helpers …

and ADR-117 §5.1 deferred `wifi-densepose-mat` (depends on nn) and the RuVector
tier for wheel-size reasons. The three SOTA subsystems that a Python researcher
most wants — re-identification embeddings, cross-environment transfer, and the
disaster-triage tool — are precisely the ones still unreachable from
`pip install wifi-densepose`.

### 1.2 The three subsystems already exist and are tested in Rust

None of this is new research. Each subsystem is a shipped, tested Rust module:

| Subsystem | ADR | Rust location (verified HEAD) | Nature |
|---|---|---|---|
| **AETHER** — contrastive CSI embedding / re-identification | ADR-024 | `wifi-densepose-sensing-server/src/embedding.rs` (`EmbeddingExtractor`, `ProjectionHead`, `CsiAugmenter`, `AetherConfig`, `aether_loss`, `info_nce_loss`, `alignment_metric`, `uniformity_metric`) | Pure-sync DSP + linear algebra; 128-dim L2-normalized embeddings |
| **MERIDIAN** — cross-environment domain generalization | ADR-027 | `wifi-densepose-train` (`domain::{DomainFactorizer, DomainClassifier, GradientReversalLayer, AdversarialSchedule}`, `geometry::{GeometryEncoder, FourierPositionalEncoding, FilmLayer, MeridianGeometryConfig}`, `rapid_adapt::{RapidAdaptation, AdaptationLoss}`, `virtual_aug::VirtualDomainAugmentor`, `eval::CrossDomainEvaluator`) + `wifi-densepose-signal::hardware_norm::{HardwareNormalizer, HardwareType, CanonicalCsiFrame}` | Inference/adaptation path is pure-Rust and **un-gated**; only `model`/`trainer`/`losses` need `tch-backend` (libtorch) |
| **MAT** — Mass Casualty Assessment Tool | (root CLAUDE.md crate table) | `wifi-densepose-mat` (`DisasterResponse`, `DisasterConfig`, `DetectionPipeline`, `EnsembleClassifier`, `TriageCalculator`, `TriageStatus`, `Survivor`, `VitalSignsReading`) | Cargo-feature-gated (`mat`); sync ingest (`push_csi_data`) + async scan loop (`start_scanning`, tokio) |

### 1.3 Why now, and why gated extras

Two forces make P6 timely: (a) the v2.0.0 wheel is stable and its abi3-py310
build matrix is proven, so adding modules is incremental; (b) integrators reading
the ADR-115/ADR-117 notes are asking for Python access to re-identification and
cross-room transfer specifically.

But pulling all three into the **default** wheel would break ADR-117 §5.4's
**≤ 5 MB per-platform wheel budget** and its "no heavy system deps" invariant:

- MAT is already cargo-`mat`-gated upstream *because* it drags in the ML/detection
  stack; the default wheel must not carry it.
- MERIDIAN's training path (`model`/`trainer`/`losses`) is `tch-backend`-gated and
  would pull libtorch (30 MB+), the exact wheel-size risk ADR-117 §5.1 flagged.

So P6 mirrors the existing `[client]` extra pattern (ADR-117 §5.6): each subsystem
becomes an **optional pip extra**, and the compiled surface is **feature-gated in
`wifi-densepose-py`'s `Cargo.toml`** so the default wheel stays lean.

### 1.4 What this ADR is *not*

- Not a port of the Rust subsystems to Python — the Rust workspace stays
  authoritative and unmodified, exactly as ADR-117 §1.3 established.
- Not the `wifi-densepose-nn` / libtorch binding (still deferred; MERIDIAN binds
  only the un-gated inference/adaptation path, not `tch-backend` training).
- Not a change to the default wheel's contents, size budget, or abi3 base.

---

## 2. Gap analysis

| Capability | Rust crate(s) | pip v2.x status | Gap severity |
|---|---|---|---|
| Extract a 128-dim re-ID embedding from a CSI window | `sensing-server::embedding` (AETHER) | Not present | **High** |
| Compare two CSI observations by learned similarity (same room? same person?) | AETHER `EmbeddingExtractor` + cosine | Not present | **High** |
| Hardware-invariant CSI normalization (ESP32 / Intel 5300 / Atheros → canonical 56) | `signal::hardware_norm` (MERIDIAN) | Not present | **High** |
| Geometry-conditioned zero-shot deployment (AP positions → FiLM) | `train::geometry` (MERIDIAN) | Not present | **Medium** |
| 10-second unlabeled few-shot room adaptation | `train::rapid_adapt` (MERIDIAN) | Not present | **Medium** |
| Cross-domain evaluation protocol (in/cross/few-shot MPJPE) | `train::eval` (MERIDIAN) | Not present | **Medium** |
| Disaster-survivor detection + START triage from CSI | `wifi-densepose-mat` | Not present | **Medium** (specialist audience) |

---

## 3. Decision

Adopt **three new optional pip extras**, each binding one SOTA subsystem into the
existing `wifi_densepose` wheel as a dedicated Python submodule, gated behind a
matching Cargo feature so the default wheel is unchanged:

```
pip install wifi-densepose              # unchanged: core + vitals + bfld (≤5 MB)
pip install wifi-densepose[aether]      # + wifi_densepose.aether
pip install wifi-densepose[meridian]    # + wifi_densepose.meridian
pip install wifi-densepose[mat]         # + wifi_densepose.mat   (mirrors upstream `mat` cargo feature)
pip install wifi-densepose[sota]        # convenience: aether + meridian + mat
```

This path is called **PIP-TRINITY**. It reuses ADR-117's established idiom
end-to-end: `#[pyclass]` newtype wrappers holding an `inner` Rust value, `#[new]`
constructors, `#[getter]` accessors, `__repr__`, a per-module `register(m)` fn,
and — critically — **GIL release via `py.allow_threads(|| …)` on every
compute-heavy call**, exactly as `bindings/vitals.rs:229` and `:293` already do.

### 3.1 Feature gating in `wifi-densepose-py`

New Cargo features and optional path-deps in `python/Cargo.toml`; each binding
module is `#[cfg(feature = "…")]`-compiled and conditionally `register()`ed in
`src/lib.rs`, so a default build links none of the three:

```toml
[features]
default = []
aether   = ["dep:wifi-densepose-sensing-server"]
meridian = ["dep:wifi-densepose-train", "dep:wifi-densepose-signal"]
mat      = ["dep:wifi-densepose-mat"]              # upstream `mat` feature flows through
sota     = ["aether", "meridian", "mat"]

[dependencies]
wifi-densepose-sensing-server = { version = "0.3.0", path = "../v2/crates/wifi-densepose-sensing-server", optional = true, default-features = false }
wifi-densepose-train          = { version = "0.3.0", path = "../v2/crates/wifi-densepose-train", optional = true, default-features = false }  # NO tch-backend
wifi-densepose-signal         = { version = "0.3.0", path = "../v2/crates/wifi-densepose-signal", optional = true }
wifi-densepose-mat            = { version = "0.3.0", path = "../v2/crates/wifi-densepose-mat", optional = true, default-features = false }
```

`[project.optional-dependencies]` in `pyproject.toml` gains `aether`, `meridian`,
`mat`, and `sota` keys mirroring the existing `client`/`dev` extras. Because each
extra changes the compiled surface, extras map to **cibuildwheel feature-flag
builds**, not pure-Python markers — the publish workflow (ADR-117 §5.4) gains a
build axis for the `[sota]` wheel variant.

### 3.2 Binding surface — AETHER (`wifi_densepose.aether`)

Backing crate: `wifi-densepose-sensing-server::embedding` (ADR-024 §2.6). The
crate is Axum/tokio-based, so we depend on it `default-features = false` and bind
**only the sync `embedding` types** — never the server/runtime. If the embedding
module cannot be reached without a tokio dependency (Open Question §11.1), the
fallback is to hoist `embedding.rs` into a leaf crate; that is a Rust-side
refactor, not a Python API change.

| Python symbol | Wraps | Signature (Python) |
|---|---|---|
| `AetherConfig` | `AetherConfig` | `AetherConfig(d_model=64, d_proj=128, temperature=0.07, vicreg_alpha=1.0, vicreg_beta=25.0, vicreg_gamma=1.0)` — frozen, `__repr__` |
| `CsiAugmenter` | `CsiAugmenter` | `CsiAugmenter(seed)`; `.augment(window: list[list[float]]) -> list[list[float]]` |
| `EmbeddingExtractor` | `EmbeddingExtractor` | `.embed(csi_features: list[list[float]]) -> list[float]` (128-dim, L2-normed); `.forward_dual(...) -> tuple[PoseEstimate, list[float]]` |
| `aether_loss(...)` | `aether_loss` | returns `AetherLossComponents(total, info_nce, variance, covariance)` — frozen dataclass-like |
| `cosine_similarity(a, b)` | thin helper | `float`; convenience for re-ID scoring (not a re-impl — calls the same dot product) |
| `alignment_metric`, `uniformity_metric` | same | `float` |

GIL strategy: `embed`, `forward_dual`, `augment`, and `aether_loss` wrap their
Rust call in `py.allow_threads(|| …)` — these are pure-sync matrix ops that touch
no Python objects, matching the vitals precedent. A single-frame `embed()` is
sub-millisecond (ADR-024 §2.8 target <1 ms FP32), but batch/augment calls exceed
the 0.5 ms GIL-release threshold ADR-117 §P3 set.

`.pyi` stubs: add `wifi_densepose/aether.pyi` declaring the five classes/functions
with precise numeric types; extend the top-level `wifi_densepose/__init__.pyi`
with a `TYPE_CHECKING`-guarded re-export so `mypy --strict` sees them only when
the extra is installed.

### 3.3 Binding surface — MERIDIAN (`wifi_densepose.meridian`)

Backing crates: `wifi-densepose-train` (inference/adaptation path, **no
`tch-backend`**) + `wifi-densepose-signal::hardware_norm`. The `model`/`trainer`/
`losses` modules are libtorch-gated and are **out of scope** — Python gets the
domain-generalization *inference and calibration* surface, not the training loop.

| Python symbol | Wraps | Signature (Python) |
|---|---|---|
| `HardwareType` | `HardwareType` | `#[pyclass(eq, eq_int, hash, frozen)]` enum: `Esp32S3 / Intel5300 / Atheros / Generic`; `HardwareType.detect(subcarrier_count) -> HardwareType` |
| `HardwareNormalizer` | `HardwareNormalizer` | `.normalize(frame: CsiFrame, hw: HardwareType) -> CanonicalCsiFrame` |
| `CanonicalCsiFrame` | `CanonicalCsiFrame` | frozen; `.amplitudes`, `.phases`, `.hardware_type` getters |
| `GeometryEncoder` | `GeometryEncoder` | `GeometryEncoder(MeridianGeometryConfig)`; `.encode(ap_positions: list[tuple[float,float,float]]) -> list[float]` (64-dim, permutation-invariant) |
| `MeridianGeometryConfig` | `MeridianGeometryConfig` | frozen config |
| `RapidAdaptation` | `RapidAdaptation` | `.calibrate(csi_windows: list[list[list[float]]]) -> AdaptationResult` (10-sec unlabeled few-shot) |
| `AdaptationResult` | `AdaptationResult` | frozen result: `.frames_used`, `.converged`, `.loss` |
| `CrossDomainEvaluator` | `CrossDomainEvaluator` | `.evaluate(...) -> dict[str, float]` (in/cross/few-shot MPJPE, domain-gap ratio) |

GIL strategy: `normalize`, `encode`, `calibrate`, and `evaluate` are wrapped in
`py.allow_threads`. `normalize` targets <50 µs/frame (ADR-027 §4.1) and `encode`
<100 µs (§4.3), but `calibrate` runs contrastive test-time training over 200
frames and is the primary GIL-release beneficiary.

`.pyi` stubs: `wifi_densepose/meridian.pyi`. `DomainFactorizer` /
`GradientReversalLayer` / `VirtualDomainAugmentor` are **training-time only** and
are *not* bound in P6 (they need the tch training loop) — Open Question §11.2
records this boundary.

### 3.4 Binding surface — MAT (`wifi_densepose.mat`)

Backing crate: `wifi-densepose-mat`, bound behind the `[mat]` extra so the
disaster/ML stack never enters the default wheel — mirroring the upstream `mat`
cargo feature exactly. `DisasterResponse::start_scanning` is async (tokio); rather
than bind an event loop, P6 binds the **sync ingest + query surface** and a
single-shot `scan_once()` helper (a sync wrapper over one `scan_cycle`, added
Rust-side if needed — see §11.3).

| Python symbol | Wraps | Signature (Python) |
|---|---|---|
| `DisasterType` | `DisasterType` | `#[pyclass(eq, eq_int, hash, frozen)]` enum: `Earthquake / BuildingCollapse / Avalanche / Flood / Mine / Unknown` |
| `TriageStatus` | `TriageStatus` | frozen enum (START protocol classes) |
| `DisasterConfig` | `DisasterConfig` | builder-style kwargs: `DisasterConfig(disaster_type, sensitivity=0.8, confidence_threshold=0.5, max_depth=5.0)` |
| `DisasterResponse` | `DisasterResponse` | `.push_csi_data(amplitudes, phases)`; `.scan_once()`; `.survivors() -> list[Survivor]`; `.survivors_by_triage(status) -> list[Survivor]` |
| `Survivor` | `Survivor` | frozen: `.id`, `.triage_status`, `.location`, `.vital_signs` getters |
| `VitalSignsReading` | `VitalSignsReading` | frozen: breathing / heartbeat / movement fields |

GIL strategy: `push_csi_data` and `scan_once` wrap the detection-pipeline call in
`py.allow_threads` — the ensemble classifier + localization are the compute-heavy
part and touch no Python state.

`.pyi` stubs: `wifi_densepose/mat.pyi`.

---

## 4. Benchmarking & the measured-vs-claimed parity requirement

A binding that "runs without crashing" is worthless if it silently regresses
accuracy versus the native Rust call. The point of P6 is to prove the Python
surface reproduces the Rust subsystem **bit-for-bit**, then to hold each binding
to the *same* published SOTA bar its ADR already claims.

### 4.1 Parity harness (bit-for-bit, mandatory)

Each subsystem ships a golden-vector parity test. A committed input fixture is
run through **both** a tiny native-Rust reference binary (in
`v2/crates/wifi-densepose-py/tests/golden/`) and the Python binding; the two
outputs must hash-match under SHA-256 (the ADR-028 / ADR-117 §5.7 witness scheme):

- `aether`: identical 128-dim embedding bytes for a fixed CSI window + fixed seed.
- `meridian`: identical `CanonicalCsiFrame` bytes for a fixed ESP32 (64-sub) and
  Intel-5300 (30-sub) frame; identical 64-dim geometry vector for fixed AP set.
- `mat`: identical triage classification + survivor count for a fixed CSI stream.

A mismatch is a **release blocker**, not a warning. This is the "MEASURED, not
CLAIMED" gate the project holds itself to.

### 4.2 pytest-benchmark micro-benchmarks

Following the existing `python/bench/test_bench_vitals.py` pattern (skipped by
default via `addopts`; run with `pytest python/bench/ --benchmark-only`):

- `python/bench/test_bench_aether.py` — steady-state `embed()` per-window cost;
  assert < 2 ms (ADR-024 §2.8 FP32 target < 1 ms with headroom) and that batched
  `embed()` scales linearly (no accidental O(n²)).
- `python/bench/test_bench_meridian.py` — `normalize()` < 200 µs/frame,
  `encode()` < 200 µs (ADR-027 §4.1/§4.3 targets ×2 headroom).
- `python/bench/test_bench_mat.py` — `scan_once()` per-cycle cost bounded by the
  configured scan interval.

### 4.3 SOTA accuracy bar the binding must reproduce (not merely run)

The parity harness (§4.1) guarantees the Python path is byte-identical to Rust, so
these published numbers are the bar the *binding output* is validated against on a
committed labeled fixture — a regression in any is a binding bug:

| Metric | Bar | Source |
|---|---|---|
| WiFlow-STD pose accuracy | **~96% PCK@20** (MEASURED-EQUIVALENT) | ADR-152 §2.2 |
| Room identification (k-NN on `env_fingerprint`) | **> 95%** | ADR-024 §2.8 |
| Person re-ID mAP | **> 80%** (WhoFi bar 95.5% on NTU-Fi) | ADR-024 §2.8, §1.5 |
| Anomaly detection F1 | **> 0.90** | ADR-024 §2.8 |
| INT8 rank correlation vs FP32 (Spearman) | **> 0.95** | ADR-024 §2.8 |
| Cross-domain MPJPE improvement | **> 20%** vs non-adversarial | ADR-027 §4.2 |
| Domain-gap ratio (cross/in-domain) | **< 1.5** | ADR-027 §4.6 |
| Few-shot MPJPE after 10-sec calibration | within **15%** of in-domain | ADR-027 §4.5 |

---

## 5. Phase ledger

```
P1  ──►  P2  ──►  P3  ──►  P4
aether   meridian  mat      docs +
bindings bindings  behind   examples
                   extra
```

### P1 — AETHER bindings (`[aether]` extra)

- [ ] Add `aether` feature + optional `wifi-densepose-sensing-server` dep
  (`default-features = false`) to `python/Cargo.toml`; resolve §11.1 tokio question.
- [ ] `python/src/bindings/aether.rs` — `AetherConfig`, `CsiAugmenter`,
  `EmbeddingExtractor`, `aether_loss` → `AetherLossComponents`, `cosine_similarity`,
  `alignment_metric`, `uniformity_metric`; `py.allow_threads` on compute paths.
- [ ] `#[cfg(feature = "aether")]` gate + conditional `register()` in `src/lib.rs`;
  add `p6-aether-bindings` to `build_features()`.
- [ ] `wifi_densepose/aether.py` facade + `aether.pyi` stub; `[aether]` extra in
  `pyproject.toml`.
- [ ] `python/tests/test_aether.py` + `python/tests/golden/aether_*` parity fixture.

### P2 — MERIDIAN bindings (`[meridian]` extra)

- [ ] Add `meridian` feature + optional `wifi-densepose-train` (**no
  `tch-backend`**) + `wifi-densepose-signal` deps.
- [ ] `python/src/bindings/meridian.rs` — `HardwareType`, `HardwareNormalizer`,
  `CanonicalCsiFrame`, `GeometryEncoder`, `MeridianGeometryConfig`,
  `RapidAdaptation` → `AdaptationResult`, `CrossDomainEvaluator`.
- [ ] Gate + register; `wifi_densepose/meridian.py` + `meridian.pyi`; `[meridian]`
  extra.
- [ ] `python/tests/test_meridian.py` + golden parity fixture (ESP32 + Intel-5300
  canonicalization, geometry vector).

### P3 — MAT bindings behind `[mat]` extra

- [ ] Add `mat` feature + optional `wifi-densepose-mat` dep; confirm the sync
  `scan_once()` surface exists Rust-side (§11.3) or add it.
- [ ] `python/src/bindings/mat.rs` — `DisasterType`, `TriageStatus`,
  `DisasterConfig`, `DisasterResponse`, `Survivor`, `VitalSignsReading`;
  `py.allow_threads` on `push_csi_data` / `scan_once`.
- [ ] Gate + register; `wifi_densepose/mat.py` + `mat.pyi`; `[mat]` + `[sota]`
  extras; add `[sota]` build axis to the cibuildwheel matrix.
- [ ] `python/tests/test_mat.py` + golden triage parity fixture.

### P4 — Docs, examples, and benchmark suite

- [ ] `python/bench/test_bench_aether.py`, `test_bench_meridian.py`,
  `test_bench_mat.py` (pytest-benchmark, §4.2).
- [ ] Parity harness wiring (§4.1) into CI as a release-blocking gate.
- [ ] `examples/reid_from_csi.py`, `examples/cross_room_calibrate.py`,
  `examples/mat_triage.py`; README extras table; `mypy --strict` over examples.
- [ ] Update ADR-117 §6 "P6+ Deferred" to point at this ADR.

### P5+ — Deferred (unchanged from ADR-117)

- [ ] `wifi-densepose-nn` / libtorch bindings (MERIDIAN training loop,
  `DomainFactorizer`, GRL) — still blocked on the libtorch wheel-size question.
- [ ] `wifi-densepose-ruvector` RuVector attention bindings.
- [ ] Matter integration helpers.

---

## 6. Acceptance criteria

All must pass before ADR-185 is Accepted. Nothing here is claimed done — this is a
proposal.

- [ ] `pip install wifi-densepose` (no extras) produces a wheel **≤ 5 MB per
  platform** — i.e. the default wheel is byte-for-byte unaffected by P6
  (`auditwheel show` links none of the three subsystems).
- [ ] `pip install wifi-densepose[aether]` then
  `pytest python/tests/test_aether.py -q` — all tests pass, including one that
  round-trips a **real 128-dim embedding vector** through `EmbeddingExtractor.embed()`
  and asserts L2-norm ≈ 1.0 and byte-identity to the golden Rust reference.
- [ ] `pytest python/tests/test_meridian.py -q` — passes, including a test that
  canonicalizes a synthetic ESP32 (64-sub) **and** Intel-5300 (30-sub) frame to 56
  subcarriers and hash-matches the native Rust `HardwareNormalizer`.
- [ ] `pip install wifi-densepose[mat]` then `pytest python/tests/test_mat.py -q` —
  passes, including a test that pushes a fixed CSI stream and asserts the triage
  classification matches the native Rust `DisasterResponse` exactly.
- [ ] `pytest python/bench/ --benchmark-only` — AETHER `embed()` < 2 ms, MERIDIAN
  `normalize()` < 200 µs, MERIDIAN `encode()` < 200 µs (steady state, reference
  machine per ADR-117 §10).
- [ ] Parity harness (§4.1): all three golden-vector SHA-256 comparisons match;
  wired as a **release-blocking** CI gate.
- [ ] SOTA-bar reproduction (§4.3): on the committed labeled fixture, the Python
  binding reproduces each cited number within measurement tolerance of the native
  Rust call (no silent accuracy regression).
- [ ] `.pyi` stubs present for all three modules; `mypy --strict` passes on
  `examples/reid_from_csi.py`, `examples/cross_room_calibrate.py`,
  `examples/mat_triage.py`.
- [ ] `python -c "import wifi_densepose.aether"` raises a clear `ImportError`
  naming the `[aether]` extra when the extra is not installed (same for
  `meridian` / `mat`).

---

## 7. Consequences

### 7.1 Positive

- **Closes the ADR-117 P6 gap**: the three most-requested SOTA subsystems become
  scriptable from Python without touching the Rust workspace.
- **Default wheel stays lean**: feature-gated extras preserve ADR-117 §5.4's ≤ 5 MB
  budget and "no heavy system deps" invariant; MAT's ML stack and MERIDIAN's
  libtorch path never enter the base wheel.
- **Reuses the proven idiom**: no new binding machinery — same `#[pyclass]` +
  `py.allow_threads` + `register()` pattern already shipping in `bindings/vitals.rs`.
- **Prove-everything alignment**: the parity harness makes "the Python binding
  equals the Rust core" a *measured, hash-verified* claim, not an assertion —
  matching the project's MEASURED-vs-CLAIMED discipline.
- **Upstream consistency**: `[mat]` pip extra mirrors the `mat` cargo feature, so
  the Python packaging story matches the Rust one exactly.

### 7.2 Negative

- **cibuildwheel matrix grows**: `[sota]` is a distinct compiled variant, adding a
  build axis (and CI time) beyond ADR-117's 5-wheel abi3 matrix.
- **AETHER's backing crate is server-shaped**: depending on
  `wifi-densepose-sensing-server` (Axum/tokio) risks pulling a runtime into an
  extension module; may force a Rust-side refactor to hoist `embedding.rs` into a
  leaf crate (§11.1).
- **MERIDIAN surface is partial**: training-time types (`DomainFactorizer`, GRL,
  `VirtualDomainAugmentor`) stay unbound until the deferred libtorch tier, so the
  Python API is inference/adaptation-only — potential user confusion (mitigated by
  docs + `.pyi` omissions).
- **Golden fixtures are maintenance surface**: any intentional numeric change in a
  Rust subsystem requires regenerating and re-witnessing its golden vector.

### 7.3 Neutral

- The `[sota]` convenience extra is purely additive; users who want one subsystem
  install one extra.
- No change to the v2.0.0 semver line; extras ship additively as v2.x.y.

---

## 8. Alternatives considered

### Alt-A: Fold all three into the default wheel

Rejected — breaks ADR-117 §5.4's ≤ 5 MB budget, drags MAT's ML stack and (via
MERIDIAN training) libtorch into every install, and contradicts the upstream
`mat` cargo-feature gating.

### Alt-B: Separate PyPI packages (`wifi-densepose-aether`, etc.)

Rejected for the SOTA trio — three packages fragment the import namespace and
duplicate the abi3/cibuildwheel setup. (This remains the right call for the
libtorch `nn` tier per ADR-117 Open Q §11.2, which is genuinely heavy.) Extras of
one wheel keep `wifi_densepose.*` coherent.

### Alt-C: Pure-Python reimplementation of the three subsystems

Rejected explicitly — this is the exact drift ADR-117 §8 Alt-C was created to
exit. A Python reimplementation would immediately begin diverging from the Rust
SOTA and could not pass the §4.1 bit-for-bit parity gate.

### Alt-D: REST/WS client to a running sensing-server for AETHER

Rejected as the primary path — provides zero offline embedding utility and cannot
host the parity harness over local Rust code (same reasoning as ADR-117 §8 Alt-B).
The pure-Python client layer (`[client]`) remains available for streaming.

---

## 9. Risks

| Risk | Likelihood | Severity | Mitigation |
|---|---|---|---|
| `wifi-densepose-sensing-server` pulls tokio into the extension module | High | High | Depend `default-features = false`; bind only `embedding` types; if unavoidable, hoist `embedding.rs` into a leaf crate (Rust refactor, no Python API change) — §11.1 |
| MERIDIAN accidentally links `tch-backend` (libtorch) via a default feature | Medium | High | Explicit `default-features = false` on `wifi-densepose-train`; CI `auditwheel`/`ldd` check that no libtorch symbol is present in the `[meridian]` wheel |
| `[sota]` build axis blows up cibuildwheel time | Medium | Medium | Build `[sota]` variant only on tagged releases, not every PR |
| Golden vectors drift when a Rust subsystem changes intentionally | Medium | Low | Documented regeneration step + ADR-028 witness re-sign; parity mismatch is a loud release blocker, never silent |
| MAT async-only surface has no clean sync entry point | Medium | Medium | Add sync `scan_once()` wrapper Rust-side (§11.3) before binding |
| Users install base wheel and expect `wifi_densepose.aether` | Low | Low | Clear `ImportError` naming the missing extra (acceptance criterion §6) |

---

## 10. Compatibility

- No change to the default wheel, its abi3-py310 base, or its size budget.
- Extras ship additively on the existing v2.x line; no semver break.
- `[mat]` pip extra ↔ `mat` cargo feature parity is preserved by construction.
- `.pyi` stubs are gated so `mypy --strict` only sees a subsystem when its extra
  is installed.

---

## 11. Open questions

1. **AETHER crate shape**: can `wifi-densepose-sensing-server::embedding` be linked
   `default-features = false` without pulling tokio/Axum into the extension module?
   If not, do we hoist `embedding.rs` into a leaf `wifi-densepose-aether` (or
   `-embedding`) crate before P1? *Tentative: attempt `default-features = false`
   first; hoist only if `auditwheel` shows a tokio link.*

2. **MERIDIAN training-time types**: `DomainFactorizer`, `GradientReversalLayer`,
   and `VirtualDomainAugmentor` are meaningful only with the tch training loop.
   Confirm they stay unbound in P6 and move with the deferred libtorch tier.
   *Tentative: yes — P6 is inference/adaptation only.*

3. **MAT sync entry point**: `DisasterResponse::start_scanning` is an async tokio
   loop. Does a sync single-cycle `scan_once()` already exist, or must it be added
   Rust-side? *Tentative: add a thin sync `scan_once()` wrapping one `scan_cycle`;
   do not bind an event loop into the extension.*

4. **`[sota]` wheel vs per-extra wheels**: cibuildwheel builds one binary per
   feature-set. Do we publish one `[sota]` wheel and let pip select, or per-extra
   wheels? This affects the number of build variants. *Tentative: single `[sota]`
   superset wheel on tagged releases; base wheel stays feature-free.*

5. **INT8 embedding path in Python**: ADR-024 §2.8 sets an INT8 rank-correlation
   bar. Do we expose the INT8 quantized `embed()` in P6, or FP32 only first?
   *Tentative: FP32 in P6; INT8 follows once the Rust quantized path is stable.*

---

## 12. References

### Internal ADRs
- **ADR-117**: pip modernization via PyO3 + maturin — the wheel this ADR extends;
  §5.1/§5.4/§5.6 (extras + wheel budget), §6 "P6+ Deferred".
- **ADR-024**: Project AETHER — contrastive CSI embedding; §2.6 module surface,
  §2.8 performance/accuracy targets.
- **ADR-027**: Project MERIDIAN — cross-environment domain generalization; §4
  phase acceptance criteria, §4.6 evaluation protocol.
- **ADR-152**: WiFi-Pose SOTA 2026 — WiFlow-STD ~96% PCK@20 MEASURED-EQUIVALENT bar.
- **ADR-028**: ESP32 capability audit / witness scheme — the SHA-256 parity gate
  the §4.1 golden harness reuses.

### Rust source (verified HEAD)
- `v2/crates/wifi-densepose-sensing-server/src/embedding.rs` — AETHER.
- `v2/crates/wifi-densepose-train/src/{domain,geometry,rapid_adapt,virtual_aug,eval}.rs` — MERIDIAN.
- `v2/crates/wifi-densepose-signal/src/hardware_norm.rs` — MERIDIAN HardwareNormalizer.
- `v2/crates/wifi-densepose-mat/src/lib.rs` — MAT.
- `python/src/bindings/vitals.rs` — the `py.allow_threads` GIL-release precedent.
- `python/bench/test_bench_vitals.py` — the pytest-benchmark pattern P4 follows.
