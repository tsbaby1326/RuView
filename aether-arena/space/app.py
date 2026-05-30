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
        return [["— no entries yet —", "be the first", "", "", ""]]
    results.sort(key=lambda r: r.get("pck20_all") or r.get("pck_all") or 0, reverse=True)
    return [[
        r.get("submitter", "?"),
        r.get("model_ref", "?"),
        r.get("tier", "?"),
        f"{(r.get('pck20_all') or r.get('pck_all') or 0):.4f}",
        (r.get("proof_sha256") or "")[:16],
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
    chain = gr.Markdown(verify_chain())

    with gr.Tab("🏆 Leaderboard"):
        cat = gr.Dropdown(["all", "pose", "presence"], value="all", label="Category")
        tbl = gr.Dataframe(
            headers=["Submitter", "Model", "Tier", "Score", "Proof (sha256…)"],
            value=leaderboard("all"), interactive=False, wrap=True,
        )
        cat.change(leaderboard, cat, tbl)
        gr.Markdown("*Benchmark-first: the board starts empty. Every row is a real harness witness — no seeded numbers.*")

    with gr.Tab("📤 Submit"):
        gr.Markdown(SUBMIT)
    with gr.Tab("🔬 Verify"):
        gr.Markdown(VERIFY)
    with gr.Tab("ℹ️ About"):
        gr.Markdown(ABOUT)

if __name__ == "__main__":
    demo.launch(server_name="0.0.0.0", server_port=7860)
