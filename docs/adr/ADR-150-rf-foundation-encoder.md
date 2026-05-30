# ADR-150: RuView RF Foundation Encoder — pose-preserving, subject/room/device-invariant CSI embedding

| Field | Value |
|-------|-------|
| **Status** | Proposed |
| **Date** | 2026-05-30 |
| **Deciders** | ruv |
| **Codebase target** | New `wifi-densepose-rfencoder` (or `nn/src/rf_foundation.rs`) + training in `wifi-densepose-train`; consumed by the MM-Fi pose head and the AetherArena Generalization Track (ADR-149) |
| **Relates to** | ADR-024 (Contrastive CSI Embedding / AETHER), ADR-027 (Cross-Environment Domain Generalization / MERIDIAN), ADR-134 (CIR), ADR-135 (calibration + coherence gate), ADR-145 (Ablation/Eval Harness), ADR-149 (AetherArena benchmark) |

---

## 1. Context

AetherArena now has a published, metric- and protocol-matched MM-Fi result: **81.63% torso-PCK@20 in-domain (random_split), exceeding MultiFormer's 72.25%** ([#876](https://github.com/ruvnet/RuView/issues/876)). But the **leakage-free cross-subject** number collapses to **~11.6% torso-PCK** (27% under the looser bbox metric). That gap is the real deployment frontier — homes, elder care, festivals, unseen bodies.

Naïve fixes already tested and **failed**: a subject-adversarial (DANN) embedding did not move cross-subject (baseline 27.26% → DANN 27.54% bbox; torso 11.57%). Bigger capacity *hurt* (transformer cross-subject 24.8% < conv 27.3%) — extra parameters overfit seen subjects.

**Conclusion:** a *generic* "better feature vector" will not help. The lever is an embedding trained for the **right invariance** — one that preserves pose while removing subject, room, and device signatures, and that *exposes* channel instability rather than hiding it.

### 1.1 Why DANN failed (and the corrected rule)

Subject identity is partly **entangled with valid pose evidence** — body scale, limb proportions, gait, RF scattering. Blindly erasing subject info also erases information the pose decoder needs. The corrected rule:

> **Remove subject identity only after preserving pose geometry.** Supervised *pose-contrast across subjects* beats naïve adversarial identity removal.

The frontier objective is **not** `same-subject = positive`. It is:

> **same pose across different subjects = positive; different pose = negative.**

## 2. Decision

**Build the RuView RF Foundation Encoder: a self-supervised, pose-preserving, subject/room/device-invariant RF representation for CSI (extensible to CIR, ADR-134, and BFLD).** Positioned as a **platform primitive**, not a benchmark trick.

### 2.1 What the embedding must keep / remove

| Signal | Action | Why |
|--------|--------|-----|
| Pose geometry | **Keep** | target signal |
| Limb-motion deltas | **Keep** | strong temporal cue |
| Subject identity | **Remove** (post-pose) | causes overfit |
| Static room multipath | **Remove** | breaks transfer |
| Device-specific phase artifacts | **Remove** | breaks cross-hardware |
| Antenna-layout quirks | **Normalize** | deployment portability |
| Channel instability | **Expose separately** | confidence gating / anti-hallucination |

### 2.2 Architecture

```
CSI frame sequence
  → physics normalization        (antenna geometry, subcarrier stability, phase-unwrap quality, room-impulse structure)
  → masked CSI encoder           (SSL: learn channel structure from unlabeled CSI — 150k home + 320k MM-Fi frames)
  → temporal contrastive encoder (motion continuity)
  → skeleton-aware pose decoder  (graph head — anatomical constraints, GraphPose-Fi style, arXiv 2511.19105)
  → confidence + coherence head  (mincut / spectral coherence as RF-integrity signal)
```

### 2.3 Training objectives (loss stack)

```
L_total = L_pose
        + 0.20 · L_masked_csi          # learn channel structure (unlabeled)
        + 0.10 · L_temporal_contrast   # motion continuity
        + 0.20 · L_pose_contrast        # same-pose-across-subjects = positive  ← the frontier
        + 0.05 · L_subject_decorrelation # remove identity only where it conflicts with pose
        + 0.10 · L_coherence            # predict when RF evidence is weak
```

Invariant target:
```
embedding ≈ pose + motion + channel-coherence
embedding ≠ subject-identity + static-room-signature + device-artifact
```

### 2.4 The RuView differentiator — auditable RF perception that knows when it's wrong

The coherence head gates pose confidence by **channel coherence**: when multipath structure changes (mincut / spectral coherence drop), the model flags low RF integrity instead of hallucinating a pose. This is the **anti-hallucination** component most WiFi-pose papers lack, and it turns RuView from a model into sensing infrastructure. (Ties to ADR-135 coherence gate.)

## 3. Experiment plan — three variants, frozen-decoder test

Same split, same decoder, same seed set; only the embedding changes.

| Variant | Description | Success threshold (cross-subject torso-PCK) |
|---------|-------------|----------------------------------------------|
| **E1** | Masked CSI pretrain | **+3** |
| **E2** | Pose-contrastive across subjects | **+6** |
| **E3** | Physics-normalized SSL + skeleton head | **+10** |

### 3.1 Expected gains (estimate)

| Method | cross-subject torso-PCK gain |
|--------|------------------------------|
| Naïve embedding | 0–2 |
| DANN adversarial | 0–3 (high collapse risk) — *empirically ~0* |
| Masked CSI pretrain | +3–8 |
| Pose-contrastive | +5–12 |
| Physics-norm + SSL + graph decoder | +10–20 |
| + more subject-diverse paired data | +20 |

Plausible trajectory: 11.6% → **20–25% near term**, **30–40% with enough subject/environment diversity**. That is a stronger research claim than squeezing random-split from 81.6% → 88%.

## 4. Acceptance Test

The encoder is accepted **only if it improves cross-subject torso-PCK@20 by ≥ 6 absolute points without reducing random-split torso-PCK@20 by more than 2 points** — on the same MM-Fi pipeline, one-command reproduction, with per-joint error tables. Results land as AetherArena witness rows (ADR-149), nothing published until reviewed.

## 5. Consequences

**Positive:** a reusable, self-supervised RF foundation encoder for CSI/CIR/BFLD; the first principled attack on the cross-subject frontier; the coherence head adds an anti-hallucination integrity signal no competitor has.

**Negative / risk:** SSL pretraining requires matching the production CSI→feature pipeline (ADR-149 §SSL note flagged the resampling-replication risk); the multi-loss stack needs careful weight tuning (DANN showed loss-imbalance can collapse training); physics normalization must be validated not to discard pose-relevant deltas.

**Neutral:** the in-domain head is unchanged; the encoder slots in front of the existing pose decoder.

## 6. Alternatives Considered

1. **Bigger model only** — tested; *hurts* cross-subject (overfits seen subjects).
2. **Naïve DANN subject-adversarial** — tested; no gain, collapse risk; entangles pose evidence.
3. **More data only (camera/ADR-079)** — complementary and ultimately necessary, but slow and out-of-band; the encoder extracts more from existing data first.

## 7. Open Questions

1. Physics-normalization spec — exact antenna/subcarrier/phase terms, validated to preserve pose deltas.
2. Masked-CSI SSL on the production feature pipeline (resampling match — see ADR-149).
3. Where the coherence/mincut integrity signal is computed (reuse ADR-135 coherence gate vs new head).
4. CIR (ADR-134) / BFLD fusion into the same encoder — phase 3.
