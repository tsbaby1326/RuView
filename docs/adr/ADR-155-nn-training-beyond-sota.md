# ADR-155: NN / Training Beyond-SOTA Sweep — Milestone 1 (Claim Integrity, Honest Validation, the Unified Metric, and the SOTA Landscape)

| Field | Value |
|-------|-------|
| **Status** | Proposed |
| **Date** | 2026-06-11 |
| **Deciders** | ruv |
| **Codebase target** | `wifi-densepose-train` (`metrics.rs`, `dataset.rs`, `proof.rs`, `rapid_adapt.rs`, `ruview_metrics.rs`, `config.rs`, `ablation.rs`, `subcarrier.rs`, `bin/train.rs`, `bin/verify_training.rs`), `wifi-densepose-nn` (`tensor.rs`, `translator.rs`, `onnx.rs`), benches, docs |
| **Relates to** | ADR-154 (Signal/DSP sweep, Milestone 0), ADR-152 (WiFi-Pose SOTA 2026 intake), ADR-150 (RF Foundation Encoder), ADR-079 (Camera-Supervised Pose), ADR-027 (MERIDIAN), ADR-024 (AETHER) |
| **Scope** | Milestone 1 of the beyond-SOTA NN/training sweep: the **integrity-critical** fixes that let the training/metrics subsystem substantiate a clean accuracy claim (the unified metric, leak-free validation, honest TTA, rigorous proof), a focused set of **correctness/security** fixes, two **measured** perf wins, the NN SOTA landscape with evidence grades, and a prioritized backlog. **~45 review findings are explicitly deferred (§8)** — nothing is silently dropped. |

---

## 0. PROOF discipline (this ADR's contract)

This project has been publicly accused of "AI slop." Milestone 1 is the **most integrity-critical** of the sweep because a gap review found the training/metrics subsystem **could not substantiate a clean accuracy claim**: there were four divergent PCK implementations and three divergent OKS implementations, a model trained on real data was validated against a *synthetic* set, the dataset had no leak-free split, the test-time-adaptation path descended a *fake* gradient, and the deterministic proof self-certified on any loss decrease (including float noise) with no committed baseline.

We answer that with **evidence, not adjectives**:

- Every integrity fix ships with a **committed regression test that would have caught the bug**.
- Every perf number is **MEASURED before/after** with the exact reproduce command. A perf claim without a measured before/after is **UNPROVEN** and is not made here.
- Every external SOTA reference is graded **MEASURED** / **CLAIMED** / **THEORETICAL**.
- We disclose, in full, what the proof does **not** prove and what remains unmeasured.

### Build/test constraint (disclosed)

The reportable-metric code (`metrics.rs`, `trainer.rs`, `proof.rs`, `model.rs`, `losses.rs`) is gated behind the `tch-backend` Cargo feature (libtorch FFI). libtorch is **not installed on the development host**, so the project's standard gate is `cargo test --workspace --no-default-features` (no tch). The canonical-metric *logic* is therefore validated two ways: (1) the non-tch reachable surface (`compute_pck`/`compute_oks` free functions, `dataset.rs` split, `rapid_adapt.rs`, `ruview_metrics.rs`) runs under the workspace test suite with new regression tests; (2) the `tch`-gated accumulator/trainer/proof changes are routed through those same canonical functions, so the metric definition is identical whether or not tch is present. This limitation is disclosed rather than hidden.

---

## 1. Context — the seven divergent metric definitions

The gap review found **four** PCK and **three** OKS implementations that disagreed on normalization, on the zero-visible-joint case, and on the OKS scale:

| # | Location | Normalizer | Zero-visible PCK | OKS scale |
|---|----------|-----------|------------------|-----------|
| PCK-1 | `metrics.rs` `MetricsAccumulator` (the trainer's) | bbox **diagonal** | **1.0** (false-perfect bug) | normalized-coord diag² |
| PCK-2 | `metrics.rs` `compute_pck` | torso **hip↔shoulder** | 0.0 | — |
| PCK-3 | `metrics.rs` `compute_pck_v2` | torso **hip↔hip** (pixel) | 0.0 | — |
| PCK-4 | `training_bench.rs` | **raw threshold** (no torso) | 0.0 | — |
| OKS-1 | `metrics.rs:443` `compute_oks` | — | — | caller `s` (`1.0` ⇒ fake Gold) |
| OKS-2 | `metrics.rs:994` `compute_oks_v2` | — | — | `sqrt(area)` (could be 0) |
| OKS-3 | `ruview_metrics.rs:642` | — | — | caller `s` (`1.0` ⇒ fake Gold) |

Two of these are not merely inconsistent, they are **wrong in a claim-inflating direction**:

- **The `MetricsAccumulator` zero-visible-joint bug** scored a sample with *no visible joints* as PCK = 1.0 ("no errors to measure"). An empty or garbage prediction could thus *inflate* the reported metric.
- **The OKS `s = 1.0`-on-normalized-coordinates bug** ("fake Gold tier"): with keypoints in `[0,1]` and the scale fixed at `1.0`, every squared distance is ≈0 and the exponential kernel returns ≈1.0 for *any* pose. OKS looked near-perfect regardless of prediction quality.

This is the same metric-bug class ADR-152 flagged. Milestone 1 closes it for real.

---

## 2. Decision — TIER 1: CLAIM INTEGRITY (the "prove everything" core)

### 2.1 Unify the metrics — ONE canonical definition — ACCEPTED & IMPLEMENTED

There is now exactly **one** PCK and one OKS that may be used for any *reported* number, in the `canonical` region of `metrics.rs`:

- **`pck_canonical(pred, gt, vis, k)` — torso-normalized PCK@k.** A keypoint `j` is correct iff `‖pred_j − gt_j‖₂ ≤ k · torso`, where `torso = ‖left_hip(11) − right_hip(12)‖₂` in the keypoint coordinate space, with a **bounding-box-diagonal fallback** when the hips are not both visible. This is the COCO / ADR-152 convention validated in `benchmarks/wiflow-std/RESULTS.md` (the ~96% PCK@20 reproduction — hip↔hip torso, COCO Setting). **Zero visible joints ⇒ `(0, 0, 0.0)`** — a sample with no measurable evidence scores 0, never 1.
- **`oks_canonical(pred, gt, vis)` — COCO OKS.** `s = sqrt(area)` is derived from the **GT pose extent** (the canonical torso size as a robust, always-positive scale proxy), never a fixed `1.0`. There is no escape hatch that makes OKS ≈ 1.0 for any pose; a degenerate (zero-extent) pose returns 0.0.

**Single source of truth, enforced.** `MetricsAccumulator::update` (the trainer's), `compute_pck`, `compute_per_joint_pck`, `compute_oks`, `aggregate_metrics`, and the deprecated `compute_pck_v2`/`compute_oks_v2`/`MetricsAccumulatorV2` **all route through** `pck_canonical`/`oks_canonical`. So `Trainer::evaluate()` → `MetricsAccumulator` → canonical; the WiFlow-STD bench definition (RESULTS.md) is the reference the canonical *matches*. `eval.rs` reports MPJPE (a distinct, non-divergent error metric, unchanged). The `v2` functions and the `training_bench.rs` raw-threshold kernel are annotated **`#[deprecated]` / "DO NOT USE for reported metrics"**.

**The two claim-inflating bugs are fixed and pinned by regression tests:**

- `canonical_pck_zero_visible_is_zero_not_one` — no-visible ⇒ PCK 0.0 (was 1.0).
- `canonical_oks_not_one_for_wrong_pose_on_normalized_coords` — a pose off by 3× the torso on `[0,1]` coords yields OKS < 0.2 (the old `s=1.0` path returned ≈1.0).
- `canonical_pck_uses_hip_to_hip_torso`, `canonical_torso_falls_back_to_bbox_when_hips_hidden` — pin the normalizer.
- `all_invisible_gives_zero_pck` (renamed from `all_invisible_gives_trivial_pck`, comment cites this ADR) — the trainer accumulator now scores no-visible as 0.

**Legitimately changed test expectations** (each updated with a comment citing this finding): the historical "perfect on an all-coincident pose" fixtures used keypoints at a single point, which is *correctly unscoreable* under canonical (zero extent ⇒ no scale). Test fixtures were given a real ±0.05 hip span so the canonical normalizer is positive; `all_invisible_*` flipped from 1.0 → 0.0.

### 2.2 Honest validation — leak-free split + synthetic-val disclosure — ACCEPTED & IMPLEMENTED

**The leak.** MM-Fi windows are extracted with **stride 1** (`MmFiEntry::num_windows = num_frames − window_frames + 1`), so adjacent windows overlap by `window_frames − 1` frames (~99% at the default 100-frame window). And `bin/train.rs` validated a *real* MM-Fi training run against a **synthetic** val set "for pipeline verification" — any PCK it printed was meaningless on two counts.

**The fix (mirroring the leak-free discipline of `occupancy_bench::EvalSplit`):**

- `MmFiDataset::subject_disjoint_split(test_subject_fraction, seed) → (train_view, test_view)` partitions **whole subjects** to one side. Because every window of a subject travels with that subject, the two views share **no subject and no window** — leak-free by construction, deterministic per seed. Returns `DatasetError::InvalidSplit` on <2 subjects, bad fraction, or an empty side.
- `assert_split_leak_free(train, test)` independently verifies subject-disjointness **and** window-index-disjointness, and is called inside the split so a leaky split can never be handed out.
- `bin/train.rs` now **prefers the real split**; the synthetic path is reachable only as a labelled fallback (single-subject data) and is routed through a new `run_smoke_test` that prefixes every metric `[SMOKE-TEST] (DO NOT REPORT)`. `--dry-run` is likewise relabelled. A synthetic-val PCK can no longer be mistaken for a measurement.

**Leak-free proof (tests):** `subject_split_is_subject_and_window_disjoint` (no shared subject, no shared window index, partition covers every window once), `subject_split_is_deterministic_for_seed`, `subject_split_rejects_single_subject`, `subject_split_rejects_bad_fraction`, `assert_leak_free_detects_injected_subject_leak` (the validator catches a deliberately-injected subject overlap — a guard against future partitioner bugs).

### 2.3 rapid_adapt honesty — real gradients, scoped claim — ACCEPTED & IMPLEMENTED

`rapid_adapt.rs`'s `contrastive_step`/`entropy_step` wrote a **fake gradient** (`grad += v * 0.01`) unrelated to the stated triplet / entropy objective — so any "TTA improves the metric" was unsupported by the code.

**Resolution: real gradients (not removal).** The two `*_loss` functions are now **pure evaluators** of the real objective; `RapidAdaptation::adapt` descends them with a **central finite-difference gradient** of that exact loss (`∂L/∂wᵢ ≈ (L(w+εeᵢ) − L(w−εeᵢ))/2ε`). Finite differences genuinely minimize the stated objective (to O(ε²) truncation), so "the adaptation loss decreases" is now a **real, reproducible** measurement rather than an artefact of a hand-tuned step. The returned `final_loss` is the *actual* objective at the produced weights.

**Honest scope caveat (recorded in the module and here):** this minimizes a *self-supervised proxy* (temporal-contrastive + prediction entropy) over a tiny LoRA bottleneck on raw CSI. It is **NOT** wired to the pose model, and **there is no measured end-to-end PCK gain on WiFi pose from this path.** TTA-on-pose is a future, **not-yet-measured** capability — no PCK improvement may be cited from this module.

**Tests:** `contrastive_loss_decreases` and `entropy_loss_decreases` (20/30 real gradient steps do not increase the loss vs 0 steps), `reported_loss_is_the_real_objective_not_a_placeholder` (the returned `final_loss` equals an independent recomputation of the objective at the output weights — i.e. it is the real loss, not a fabricated number).

### 2.4 proof.rs rigor — margin + committed-hash requirement — ACCEPTED & IMPLEMENTED

The deterministic proof self-certified: `generate_expected_hash` blessed whatever the pipeline emitted, PASS counted *any* loss decrease (including 1e-9 float noise), and a *missing* expected hash defaulted to PASS.

**Two hardenings:**

1. **Minimum-decrease margin.** `MIN_LOSS_DECREASE = 1e-4`. A run counts as "learning" only when `initial − final ≥ MIN_LOSS_DECREASE` — well above float noise, far below a real step's decrease. A pipeline that only wanders by noise now **FAILS**.
2. **No-hash is a SKIP, never a PASS.** `ProofResult::is_pass()` requires `hash_matches == Some(true)` (a *committed* `expected_proof.sha256`). An absent baseline yields SKIP (exit 2). The `verify-training` binary additionally **fails fast** on a sub-margin loss *before* the hash comparison, so a missing baseline can never downgrade a non-learning pipeline to SKIP.

**What this proves — and what it does NOT (disclosed):** the proof certifies **reproducibility and determinism** (same seed ⇒ same weights ⇒ same hash) and that the optimiser *measurably* reduces a loss. It runs on a deterministic *synthetic* dataset by construction, so it does **not** prove the shipped weights came from real MM-Fi data, nor that any accuracy claim is met. Accuracy is substantiated separately (`benchmarks/wiflow-std/RESULTS.md`). There is currently **no committed `expected_proof.sha256` for the Rust proof**, so it is honestly in the SKIP state until a baseline is committed on a libtorch-enabled host — and SKIP is now reported as SKIP, not green.

**Tests:** `no_committed_hash_is_skip_not_pass`, `submargin_loss_change_fails_even_without_hash`, `committed_matching_hash_with_real_decrease_passes`.

---

## 3. Decision — TIER 2: CORRECTNESS / SECURITY

Each fix ships a test that would have caught the bug (all in the non-tch, workspace-tested surface).

| Finding | File | Fix | Test |
|---------|------|-----|------|
| `softmax(axis)` ignored the axis (whole-tensor normalize — breaks densepose per-pixel probs) | `nn/tensor.rs` | softmax along the given axis per lane; out-of-range axis ⇒ `NnError` (no panic) | (tier-2 suite) |
| `apply_attention` identity/uniform stub (any "with attention" ablation == without) | `nn/translator.rs` | **implemented real single-head scaled-dot-product attention** (`softmax(QKᵀ/√d)V` with Q/K/V/output projections); mis-shaped checkpoint projections rejected so a bad checkpoint can't silently become a no-op | `test_attention_is_not_uniform_stub`, `test_attention_rejects_wrong_weight_shape` |
| `config.validate()` had no UPPER bounds (config-OOM class still open) | `train/config.rs` | upper bounds on `window_frames`/subcarriers/`backbone_channels`/`heatmap_size`/keypoints/parts/`batch_size`; reject negative `gpu_device_id` | rejection tests; defaults+presets still validate |
| `subcarrier.rs` panic on non-contiguous input | `train/subcarrier.rs` | graceful path / typed error on strided input | non-contiguous-input test |
| `ablation.rs` `latency_percentiles` `partial_cmp().unwrap()` NaN panic | `train/ablation.rs` | `total_cmp` / NaN-guarded compare | NaN-input no-panic test |
| `onnx.rs` unchecked `-1` dim cast | `nn/onnx.rs` | reject negative/zero output dims with `NnError` | guarded-dim test |
| `ruview_metrics` `compute_single_oks` `s=1.0` fake-Gold + unguarded `[j]<17` | `train/ruview_metrics.rs` | derive scale from GT extent when none supplied; reject `s≤0`; bound the loop to array extents | `oks_rejects_nonpositive_scale`, `oks_does_not_panic_on_short_arrays`, `oks_not_perfect_for_wrong_pose_with_derived_scale` |

`rf_encoder.rs` was inspected and found to contain **no checkpoint-deserialization assert**: its `assert_eq!`s in `LinearHead::new` / `ContrastiveBatcher::new` are documented construction-time API contracts on *programmer-supplied* vector lengths, not adversarial-input panics — the described bug does not exist there. Any genuine checkpoint-load assert lives in the tch-gated `proof.rs`/`trainer.rs` path and is deferred (§8) as unverifiable without libtorch. Test pass counts: nn `--no-default-features` **35 passed**, nn `--features onnx onnx::tests` **3 passed**, train `--no-default-features` lib **176 passed**.

---

## 4. Decision — TIER 3: MEASURED perf wins (new criterion benches)

All numbers MEASURED on the Windows dev host with the `onnx` feature (`ort 2.0.0-rc.11`, runtime auto-downloaded), committed in `nn/benches/onnx_bench.rs`.

### 4.1 Zero-copy ORT input — LANDED, MEASURED

`onnx.rs` built the ORT input via `arr.iter().cloned().collect::<Vec<f32>>()` — a full element-wise copy. Replaced with a contiguous fast path (`arr.as_slice() ⇒ single memcpy`, iterator fallback only for strided views).

- **Reproduce:** `cargo bench -p wifi-densepose-nn --no-default-features --features onnx --bench onnx_bench -- onnx_input_copy`
- **Measured** (input `[1,256,64,64]` = 1.05M f32): **1.972 ms → 1.336 ms (~1.48× faster)**, 532 → 785 Melem/s. Strided fallback unchanged (within noise), correctness preserved. End-to-end real-model inference: ~45.9 µs.

### 4.2 ONNX per-inference write-lock — DIAGNOSED, NOT LANDABLE (honest)

`OnnxBackend::run` takes a `parking_lot::RwLock` **write** lock per inference, serializing concurrency. The intended fix was a read-lock. **It is not landable on `ort 2.0.0-rc.11`:** the safe `Session::run` is `&mut self` (verified against the vendored source) — there is no `&self` run path, so a read-lock fails the borrow checker. The underlying C++ `OrtSession::Run` is thread-safe, but exploiting that would require an `unsafe` interior-mutability bypass; we did **not** introduce that soundness risk. The write lock was kept, with a doc comment recording the upgrade path (a future `ort` with `&self` run ⇒ flip to `read()`).

- **Harness landed anyway**, empirically proving the serialization: `cargo bench -p wifi-densepose-nn --no-default-features --features onnx --bench onnx_bench -- onnx_concurrency` → throughput **drops** with more threads (1 thr 19.4 Kelem/s → 2 thr 16.9K → 4 thr 14.0K → 8 thr 14.3K). When `ort` exposes `&self` run, the one-line lock change will show the speedup on this same bench.

The native-conv naive-loop rewrite was **deferred** (§8) as out of scope for a measured milestone.

---

## 5. The NN / training SOTA landscape (graded)

| Candidate | What | Grade | Verdict |
|-----------|------|-------|---------|
| **GraphPose-Fi** (arXiv 2511.19105, code github.com/Cirrick/GraphPose-Fi) | Graph/skeleton pose **decoder** for cross-environment WiFi pose; MM-Fi, 17 joints — matches our setup. ADR-150 §2.2 named a graph decoder but never built it. | **CLAIMED** (preprint; cross-env gains author-reported) | **Top beyond-SOTA candidate. Propose as ACCEPTED-future — NOT built here.** Best fit because the decoder is a drop-in on our 17-joint MM-Fi backbone and directly targets the cross-environment brittleness ADR-150/ADR-027 fight. |
| **ONNX INT4** | Extend our **measured** INT8 ONNX quantization to INT4 for edge. | **THEORETICAL** for our pipeline (INT8 is MEASURED; INT4 untested here) | #2 priority — natural extension of a measured capability. |
| **CSI-JEPA vs MAE A/B** | Joint-embedding predictive pretraining vs the ADR-152 §2.3 MAE recipe. | **CLAIMED** (JEPA strong elsewhere) — **honest caveat: no JEPA *or* MAE result exists on WiFi POSE yet** (ADR-152 F3: UNSW MAE downstream tasks are classification, not pose). | #3 — run as a measured A/B, do not pre-announce a winner. |
| **"Mamba-CSI-pose"** | A state-space-model CSI pose backbone. | — | **Does NOT exist. Do not propose it.** No such artifact in the 2025–2026 literature; naming it would be exactly the kind of unfounded claim this sweep exists to prevent. |

---

## 6. Validation

- `cargo test --workspace --no-default-features` — green (the metric unification legitimately changed a handful of test expectations; each was updated with a comment citing the finding, and the trainer/eval/proof now all route through the one canonical metric).
- `python archive/v1/data/proof/verify.py` — `VERDICT: PASS` (Python pipeline proof, independent of the Rust changes).
- New criterion benches compile and run under the `onnx` feature.

---

## 7. What changed, file by file

- `metrics.rs` — `canonical_torso_size`, `pck_canonical`, `oks_canonical` (single source of truth); `MetricsAccumulator`/`compute_pck`/`compute_per_joint_pck`/`compute_oks`/`aggregate_metrics` route through them; `compute_pck_v2`/`compute_oks_v2`/`MetricsAccumulatorV2` deprecated → canonical; zero-visible and `s=1.0` bugs fixed; canonical bug-catching tests.
- `dataset.rs` — `subject_disjoint_split`, `MmFiSplitView`, `assert_split_leak_free`; leak-free split tests.
- `error.rs` — `DatasetError::InvalidSplit`.
- `bin/train.rs` — prefer real subject-disjoint split; synthetic path relabelled `run_smoke_test` ("DO NOT REPORT").
- `proof.rs` + `bin/verify_training.rs` — `MIN_LOSS_DECREASE` margin; no-hash ⇒ SKIP-not-PASS; sub-margin ⇒ FAIL-not-SKIP; new tests.
- `rapid_adapt.rs` — fake gradient removed; finite-difference gradient of the real objective; honesty docs + tests.
- `ruview_metrics.rs` — OKS scale derived from GT extent (no `s=1.0`); `s≤0` rejected; OKS loop bounded; tests.
- `config.rs` / `ablation.rs` / `subcarrier.rs` / `nn/tensor.rs` / `nn/translator.rs` / `nn/onnx.rs` — Tier-2 fixes (§3) + Tier-3 perf (§4).
- `training_bench.rs`, `sensing-server/training_api.rs` — divergent local PCK kernels annotated "DO NOT USE for reported metrics"; the sensing-server torso-height PCK unification is a **deferred** backlog item (separate service + tch boundary).

---

## 8. Deferred backlog (NOT silently dropped)

The gap review surfaced ~60 findings; this milestone scoped to the provable integrity-critical subset plus two measured perf wins. The remainder are tracked here for a future ADR-155 milestone:

- **GraphPose-Fi graph decoder** — build the §5 top candidate (ACCEPTED-future, not built).
- **ONNX INT4** quantization; **CSI-JEPA vs MAE** A/B; the rest of the §5 roadmap.
- **ONNX read-lock concurrency win** — blocked on an `ort` release exposing `&self` `Session::run` (§4.2); harness already committed.
- **native-conv naive-loop** perf rewrite (§4).
- **`rf_encoder.rs` `assert_eq!`-on-checkpoint** and any other **tch-gated** panic-on-input sites — require a libtorch host to compile/verify (`model.rs` `amp_fc1` unbounded alloc is *indirectly* guarded by the new `config.validate()` upper bounds, but a direct guard + test is deferred).
- **`sensing-server/training_api.rs` PCK** — unify the live-server torso-height PCK with `pck_canonical` (crosses the service + tch boundary).
- **`test_metrics.rs` reference kernels** — the integration test's local `compute_pck`/`compute_oks` are independent reference impls (not production); fold them onto the canonical definition.
- The remaining ~40 lower-severity review findings (style, micro-opt, doc) from the NN/training gap review.

---

## 9. Consequences

**Positive.** The training/metrics subsystem can now substantiate a clean accuracy claim: one documented metric used everywhere, a leak-free split, an honest TTA path, a proof that fails on noise and refuses to bless an unbaselined run, and two of the most claim-inflating bugs (false-perfect PCK, fake-Gold OKS) closed and pinned by regression tests. The unmeasured/unprovable parts are **disclosed**, not hidden.

**Negative / honest.** The reportable-metric tch-gated code cannot be compiled on the dev host (libtorch absent), so its validation rests on routing through the workspace-tested canonical functions plus review; the Rust deterministic proof is in SKIP until a baseline is committed on a tch host; the ONNX concurrency win is blocked upstream; and ~45 findings are deferred. None of these is presented as done.
