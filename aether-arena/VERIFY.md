# Verifying AetherArena (you don't have to trust us)

AA's credibility rests on a stranger being able to reproduce a score and see that the rules are fair. This is the **launch gate** (ADR-149 §7): v0 does not ship until all five checks below pass for someone with no insider access.

## The open scorer

The scoring engine is a pure-Rust, GPU-free binary: `aa_score_runner` in `wifi-densepose-train`. It runs the real `ruview_metrics` pose-acceptance harness on a fixed fixture and emits a cross-platform-stable SHA-256 **determinism proof**.

### Reproduce the determinism hash locally

```bash
cd v2
# Verify the committed expected hash still matches (this is the CI gate):
cargo run -q -p wifi-densepose-train --bin aa_score_runner --no-default-features
# → prints the score, the proof sha256, and "VERDICT: PASS"

# See the leaderboard-ledger row as JSON:
cargo run -q -p wifi-densepose-train --bin aa_score_runner --no-default-features -- --json
```

The expected hash is committed at [`fixtures/expected_score.sha256`](fixtures/expected_score.sha256). Same harness version + same fixture → same hash on glibc / MSVC / Apple. If your local run prints `VERDICT: PASS`, you have reproduced the scorer.

### What happens if the scoring maths changes

Any edit to `ruview_metrics.rs`, `ablation.rs`, or `aa_score_runner.rs` moves the hash and **fails the CI gate** (`.github/workflows/aether-arena-harness.yml`) until the maintainer regenerates and reviews:

```bash
cargo run -p wifi-densepose-train --bin aa_score_runner --no-default-features -- --generate-hash \
  > aether-arena/fixtures/expected_score.sha256
```

So a scorer change is always a reviewed, public diff — never silent. That's `harness_version` pinning + `determinism_gate` in action (ADR-149 §2.4–§2.5).

## The five-step acceptance test (v0 launch gate)

A stranger must be able to:

1. **Submit** a model (artifact + `schema/aa-submission.toml`) with no insider help.
2. **Get a deterministic score** — same model + same `harness_version` → same numbers.
3. **See the signed row** appended to the public results ledger.
4. **Rerun the scorer locally** on the public smoke split and reproduce the logic (the command above).
5. **Understand why the rank is fair** — private split, open scorer, pinned version, proof hash — from these docs alone.

If any step fails, v0 is not ready.

## Current status

- ✅ Step 4 (rerun the open scorer locally, reproduce the hash) — **works today** via `aa_score_runner`.
- ✅ CI harness gate runs the scorer on every PR.
- ⏳ Steps 1–3, 5 (HF Space submission flow + signed ledger) — in progress; require the HF Space deploy (needs an HF token / maintainer authorization).
