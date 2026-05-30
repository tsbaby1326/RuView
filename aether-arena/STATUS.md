# AetherArena — Build Status

Tracks ADR-149 implementation milestones. "Complete" = benchmark **infrastructure** done,
tested, CI-gated, deploy-ready, RuView baseline entered, §7 acceptance test passing.
Model **SOTA** (e.g. MM-Fi PCK@20 ~72%) is a separate long-running ML effort, blocked on
ADR-079 camera-ground-truth collection — *not* an infra-completion blocker.

| # | Milestone | Status |
|---|-----------|--------|
| M1 | ADR-149 Accepted + committed | ✅ done |
| M2 | Deterministic scorer runner (`aa_score_runner`) → tier + proof hash | ✅ done — builds `--no-default-features`, hash stable, VERDICT: PASS |
| M3 | CI harness-gate workflow (PR runs the scorer) | ✅ done — `.github/workflows/aether-arena-harness.yml` |
| M4 | Scaffold: README + submission schema + VERIFY (acceptance test) | ✅ done |
| M5 | Public smoke split (committed) + private MM-Fi held-out split prep | ⏳ next |
| M6 | HF Space (Gradio) submission flow + sandboxed scorer container | ⛔ blocked — needs HF token / maintainer authorization to deploy |
| M7 | Signed append-only Parquet results ledger | ⏳ |
| M8 | RuView baseline entry (honest PCK@20) + public launch | ⏳ |

## Blockers / decisions needed
- **HF deploy (M6)** needs an HF token and authorization to create the public `ruvnet/aether-arena` Space.
- **MM-Fi is CC BY-NC** → AA must stay non-commercial / legally distinct from the commercial RuView product.
- **Realism of M2 fixture**: current fixture is a *determinism* fixture (stable hash), not a realistic baseline; M5 swaps in real MM-Fi held-out scoring.
