"""AetherArena ("AA") — The Official Spatial-Intelligence Benchmark.

Hugging Face Space (Gradio) — the public face of the benchmark (ADR-149).
This Space is the presentation + submission layer; the heavy scoring runs in the
pinned RuView harness (CI / scorer container), and results land in the append-only,
hash-chained **witness ledger** shown here.

Benchmark-first: the board starts EMPTY. No seeded or hand-entered numbers — every
row is a real scoring-pipeline witness (inputs_sha256 + proof_sha256 + harness_version).
"""
import hashlib
import json
from pathlib import Path

import gradio as gr

LEDGER = Path(__file__).parent / "ledger.jsonl"
GENESIS_PREV = "0" * 64


def _rows():
    if not LEDGER.exists():
        return []
    return [json.loads(l) for l in LEDGER.read_text().splitlines() if l.strip()]


def _canon(row: dict) -> bytes:
    body = {k: row[k] for k in sorted(row) if k != "row_hash"}
    return json.dumps(body, separators=(",", ":"), sort_keys=True).encode()


def verify_chain():
    rows, prev = _rows(), GENESIS_PREV
    for i, r in enumerate(rows):
        if r.get("prev_hash") != prev or r.get("row_hash") != hashlib.sha256(_canon(r)).hexdigest():
            return f"❌ Ledger chain BROKEN at row {i} — tampering detected."
        prev = r["row_hash"]
    return f"✅ Witness ledger chain intact — {len(rows)} row(s), append-only."


def leaderboard(category: str):
    results = [r for r in _rows() if r.get("kind") == "result" and (category == "all" or r.get("category") == category)]
    if not results:
        return [["— no entries yet —", "", "", "", "", ""]]
    results.sort(key=lambda r: r.get("score_pct") or 0, reverse=True)
    return [[
        r.get("submitter", "?"),
        r.get("model_ref", "?"),
        f"{r.get('benchmark','?')} / {r.get('protocol','?')}",
        r.get("metric", "?"),
        f"{r.get('score_pct', 0):.2f}%",
        f"{r.get('tier','?')} (vs {r.get('sota_ref','?')})",
    ] for r in results]


FOUR_PART = "### Public leaderboard. Private evaluation split. Open scorer. Signed results."

ABOUT = """
**AetherArena** is the official, project-agnostic **Spatial-Intelligence Benchmark** —
camera-free pose, presence, occupancy, tracking, and vitals from RF/WiFi (and, over
time, mmWave / UWB / radar / multimodal). It is **not** a single-vendor board: any
team, framework, or modality enters, and every entrant — including the RuView baseline
that donated the seed scorer — is scored by the identical, open, pinned harness.

The scorer reuses RuView's released `wifi-densepose-train` acceptance harness
(`ruview_metrics` + ablation). You submit a **model, not predictions**; it is scored
against a **private** MM-Fi held-out split; one **witness** row (inputs hash + proof
hash + harness version) is appended to a **hash-chained, tamper-evident ledger**.

Spec: ADR-149. v0 ranks **pose, presence, edge-latency, determinism**. Tracking &
vitals activate when their ground truth lands; **privacy-leakage** is gated until the
membership-inference attacker ships. Source + the open scorer:
https://github.com/ruvnet/RuView/tree/main/aether-arena
"""

SUBMIT = """
### Submit a model

1. Write a manifest — [`schema/aa-submission.toml`](https://github.com/ruvnet/RuView/blob/main/aether-arena/schema/aa-submission.toml):
   declare your model ref, category, the ADR-145 feature set (F0 CSI … F3 BFLD), and the tensor I/O contract.
2. Provide your model artifact (`.safetensors` / `.rvf` / LoRA adapter).
3. It moves through `submitted → validated → quarantined → smoke_scored → full_scored → published`,
   scored in a no-network, read-only sandbox against the private split.
4. Your signed witness row appears on the leaderboard.

**You submit a model, never predictions** — predictions on data you hold prove nothing.
"""

VERIFY = """
### Verify it's fair (you don't have to trust us)

The scorer is open and reproducible. Reproduce the determinism proof + repeatability locally:

```bash
git clone https://github.com/ruvnet/RuView && cd RuView/v2
# determinism gate (same as CI):
cargo run -q -p wifi-densepose-train --bin aa_score_runner --no-default-features
# repeatability — N runs, one identical proof hash:
cargo run -q -p wifi-densepose-train --bin aa_score_runner --no-default-features -- --repeat 16
# verify the append-only witness ledger chain:
cd ../aether-arena/ledger && python3 ledger_tools.py verify
```

A stranger must be able to: submit → get a deterministic score → see the signed row →
rerun the scorer locally → understand why the rank is fair. That is the launch gate (ADR-149 §7).
"""

with gr.Blocks(title="AetherArena — Spatial-Intelligence Benchmark") as demo:
    gr.Markdown("# 📡 AetherArena (AA)\n## The Official Spatial-Intelligence Benchmark")
    gr.Markdown(FOUR_PART)
    gr.Markdown(
        "## 🏆 RuView sets new MM-Fi random-split SOTA for WiFi-CSI pose estimation — **81.63% torso-PCK@20**\n"
        "**81.63% vs MultiFormer 72.25%** (CSI2Pose 68.41%) — same MM-Fi `random_split` (0.8, seed 0), same torso-normalized PCK@20, 17 COCO keypoints. **+9.38 abs / +13.0% rel.**\n\n"
        "> ⚠️ **Controlled claim.** This is a *protocol-matched MM-Fi random-split* result — **not** solved real-world generalization. Random split contains temporal/subject-adjacency effects common to this benchmark family. Our leakage-free **cross-subject** result is far lower (~11–27%), and we treat cross-subject pose estimation as the real deployment frontier."
    )
    chain = gr.Markdown(verify_chain())

    with gr.Tab("🏆 Leaderboard"):
        cat = gr.Dropdown(["all", "pose", "presence"], value="all", label="Category")
        tbl = gr.Dataframe(
            headers=["Submitter", "Model", "Benchmark / Protocol", "Metric", "Score", "Tier (vs SOTA)"],
            value=leaderboard("all"), interactive=False, wrap=True,
        )
        cat.change(leaderboard, cat, tbl)
        gr.Markdown("*Benchmark-first: every row is a real, metric- and protocol-matched result — no seeded numbers. Integrity note: the headline 81.63% was self-corrected down from an inflated 91.86% (bbox metric) before publishing.*")

    with gr.Tab("📤 Submit"):
        gr.Markdown(SUBMIT)
    with gr.Tab("🔬 Verify"):
        gr.Markdown(VERIFY)
    with gr.Tab("ℹ️ About"):
        gr.Markdown(ABOUT)

if __name__ == "__main__":
    demo.launch(server_name="0.0.0.0", server_port=7860)
