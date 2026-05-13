# ADR-096: rvCSI — Crate Topology, the napi-c Shim, and the napi-rs Node Surface

| Field | Value |
|-------|-------|
| **Status** | Proposed |
| **Date** | 2026-05-12 |
| **Deciders** | ruv |
| **Codename** | **rvCSI** — RuVector Channel State Information runtime |
| **Relates to** | ADR-095 (rvCSI platform — D1 Rust core, D2 C-at-the-boundary, D3 TS SDK, D4 napi-rs, D5 normalized schema, D6 validate-before-FFI, D15 plugin adapters), ADR-009/ADR-040 (WASM runtimes), ADR-049 (cross-platform WiFi interface detection) |
| **PRD** | [rvCSI Platform PRD](../prd/rvcsi-platform-prd.md) |
| **Domain model** | [rvCSI Domain Model](../ddd/rvcsi-domain-model.md) |
| **Implements** | `v2/crates/rvcsi-core`, `rvcsi-dsp`, `rvcsi-events`, `rvcsi-adapter-file`, `rvcsi-adapter-nexmon`, `rvcsi-ruvector`, `rvcsi-node`, `rvcsi-cli` |

---

## 1. Context

ADR-095 set the platform-level invariant `C → Rust → TypeScript` and the fifteen decisions that constrain rvCSI. This ADR makes the *implementation* concrete: which crates exist, what each owns, where the two FFI seams are (the **napi-c** C shim below Rust, and the **napi-rs** Node addon above it), and the rules that keep `unsafe` confined and the boundary objects validated.

The two seams:

- **napi-c** — the *downward* seam to fragile vendor/firmware/driver code. Per ADR-095 D2, C is the only language allowed here, and only as a thin, allocation-free, bounds-checked shim. The Nexmon family is the first consumer.
- **napi-rs** — the *upward* seam to Node.js/TypeScript. Per ADR-095 D3/D4, the Rust runtime is exposed to JS via [napi-rs](https://napi.rs/); nothing crosses this seam that hasn't been validated (D6) and normalized (D5).

Both seams are *narrow on purpose*: everything in between — parsing, validation, DSP, windowing, event extraction, RuVector export — is safe Rust (`#![forbid(unsafe_code)]` in every crate except `rvcsi-adapter-nexmon`, which needs `extern "C"`).

---

## 2. Decision

### 2.1 Crate topology

Eight new workspace members under `v2/crates/`:

| Crate | `unsafe`? | Depends on | Owns |
|-------|-----------|------------|------|
| `rvcsi-core` | no (`forbid`) | — (serde, thiserror) | The normalized schema (`CsiFrame`/`CsiWindow`/`CsiEvent`), `AdapterProfile`, the `CsiSource` plugin trait, id newtypes + `IdGenerator`, `RvcsiError`, and the `validate_frame` pipeline + quality scoring. The shared kernel. |
| `rvcsi-dsp` | no (`forbid`) | `rvcsi-core` | Reusable DSP stages (DC removal, phase unwrap, smoothing, Hampel/MAD outlier filter, sliding variance, baseline subtraction) and scalar features (motion energy, presence score, confidence, heuristic breathing-band estimate), plus a non-destructive `SignalPipeline::process_frame`. |
| `rvcsi-events` | no (`forbid`) | `rvcsi-core` | `WindowBuffer` (frames → `CsiWindow`), the `EventDetector` trait + presence/motion/quality/baseline-drift state machines, and `EventPipeline` (windows → `CsiEvent`s). |
| `rvcsi-adapter-file` | no (`forbid`) | `rvcsi-core` | The `.rvcsi` capture format (JSONL: a header line + one `CsiFrame` per line), `FileRecorder`, and `FileReplayAdapter` (a `CsiSource`) — deterministic replay (D9). |
| `rvcsi-adapter-nexmon` | **yes** (FFI only) | `rvcsi-core` + the C shim | The **napi-c** seam: `native/rvcsi_nexmon_shim.{c,h}` compiled via `build.rs`+`cc`, a documented `ffi` module wrapping it, and `NexmonAdapter` (a `CsiSource`). |
| `rvcsi-ruvector` | no (`forbid`) | `rvcsi-core` | The RuVector RF-memory bridge: deterministic `window_embedding`/`event_embedding`, the `RfMemoryStore` trait, and `InMemoryRfMemory` + `JsonlRfMemory` (a standin until the production RuVector binding lands). |
| `rvcsi-node` | no (`deny(clippy::all)`) | all of the above | The **napi-rs** seam: the `.node` addon (cdylib + rlib) exposing a safe TS-facing surface; `build.rs` runs `napi_build::setup()`. |
| `rvcsi-cli` | no | core, dsp, events, adapter-file, adapter-nexmon, ruvector | The `rvcsi` binary: `inspect`, `replay`, `health`, `export`, `calibrate`, `stream` (ADR-095 FR7). |

`rvcsi-events` does **not** call into `rvcsi-dsp`: window statistics are simple enough to compute in `WindowBuffer` itself, and keeping the two leaves independent removes a coordination point. Higher layers (the daemon, `rvcsi-node`, `rvcsi-cli`) wire `SignalPipeline::process_frame` → `WindowBuffer::push` when they want cleaned frames.

The TypeScript SDK (`@ruv/rvcsi`) and the MCP tool server (`rvcsi-mcp`) and the long-running daemon (`rvcsi-daemon`) are *not* in this ADR's scope; they sit on top of `rvcsi-node` / the crates above and are tracked as follow-ups.

### 2.2 The napi-c shim — record format and contract

`native/rvcsi_nexmon_shim.{c,h}` is the only C in the runtime. It parses (and, for the recorder and tests, writes) a compact, byte-defined **"rvCSI Nexmon record"** — a normalized superset of the nexmon_csi UDP payload (magic, RSSI, chanspec, then interleaved `int16` I/Q in Q8.8 fixed point):

```
off  size  field
  0     4  magic = 0x52564E58 ('R','V','N','X')
  4     1  version = 1
  5     1  flags  (bit0 rssi present, bit1 noise floor present)
  6     2  subcarrier_count N        (1 .. 2048)
  8     1  rssi_dbm  (int8, valid iff flags bit0)
  9     1  noise_dbm (int8, valid iff flags bit1)
 10     2  channel       (uint16)
 12     2  bandwidth_mhz (uint16)
 14     2  reserved (0)
 16     8  timestamp_ns  (uint64)
 24   4*N  N pairs of int16 (i, q), Q8.8 fixed point
total = 24 + 4*N
```

Contract:

- **Allocation-free, global-free.** Every read is bounds-checked against the caller-supplied length; nothing can scribble outside caller buffers.
- **Structured errors, never panics.** `rvcsi_nx_parse_record` returns one of a small set of `RvcsiNxError` codes (`TOO_SHORT`, `BAD_MAGIC`, `BAD_VERSION`, `CAPACITY`, `TRUNCATED`, `ZERO_SUBCARRIERS`, `TOO_MANY_SUBCARRIERS`, `NULL_ARG`); `rvcsi_nx_strerror` maps each to a static string.
- **ABI versioned.** `rvcsi_nx_abi_version()` returns `major<<16 | minor`; the Rust side `debug_assert`s the major matches the header it was compiled against.
- The Rust `ffi` module wraps these in safe functions (`record_len`, `decode_record`, `encode_record`, `shim_abi_version`); the `unsafe` blocks are limited to the FFI calls themselves and each carries a `// SAFETY:` comment, per the project rule.

A real Nexmon deployment feeds the UDP stream (or a PCAP demux) of these records to `NexmonAdapter::from_bytes`; `from_file` reads a capture dump. Production live capture (binding the UDP socket, monitor mode, firmware patch hooks) is a later increment that reuses the same record contract — the shim's job is the *parse*, not the *socket*.

### 2.3 The napi-rs surface — what crosses the seam

`rvcsi-node` is a `["cdylib", "rlib"]` crate (cdylib = the `.node` addon; rlib so `cargo test --workspace` can link and test the Rust side without Node). Rules:

- **Only normalized/validated data crosses.** The boundary types are JS-friendly mirrors of `CsiFrame`/`CsiWindow`/`CsiEvent`/`AdapterProfile`/`SourceHealth`, or plain JSON strings — never raw pointers, never `Pending` frames. A frame is run through `rvcsi_core::validate_frame` before it is handed to JS.
- **Errors map to JS exceptions** via napi-rs's `Result` integration; `RvcsiError`'s `Display` is the message.
- **The build emits link args + `index.d.ts`/`index.js`** via `napi_build::setup()` in `build.rs`; the `@ruv/rvcsi` npm package wraps the prebuilt addon and re-exports the generated `.d.ts`.
- The addon also re-exports `nexmon_shim_abi_version()` so a JS caller can confirm the linked napi-c shim's ABI.

### 2.4 Build & test invariants

- `cargo build --workspace` and `cargo test --workspace --no-default-features` (the repo's pre-merge gate) must stay green; the new crates add tests and don't regress the existing 1,031+.
- `rvcsi-node` stays a workspace *member* (not `exclude`d like `wifi-densepose-wasm-edge`): on Linux/macOS a napi cdylib links fine with Node symbols left undefined (resolved at addon-load time), so `cargo build`/`cargo test` work without a Node toolchain. Only `napi build` (npm packaging) needs Node.
- No new heavy dependencies in the rvCSI crates: `serde`, `serde_json`, `thiserror`, `cc` (build only), `napi`/`napi-derive`/`napi-build`, `clap` (CLI only), `tempfile` (dev only). DSP math is hand-rolled — no `ndarray`/`rustfft`.

---

## 3. Consequences

**Positive**

- The two FFI seams are small, audited, and independently testable: the C shim round-trips through Rust tests; the napi surface tests run under `cargo test` without Node.
- `unsafe` is confined to one crate (`rvcsi-adapter-nexmon`) and within it to one module (`ffi`), every block documented.
- Each leaf crate (`rvcsi-dsp`, `rvcsi-events`, `rvcsi-adapter-file`, `rvcsi-ruvector`) depends only on `rvcsi-core`, so they can evolve (and be reviewed, and be swarm-implemented) independently.
- The `.rvcsi` JSONL capture format and the `JsonlRfMemory` standin make the whole pipeline runnable and testable end-to-end before any hardware or the real RuVector binding exists.

**Negative / costs**

- A `cc`-built C library means a C toolchain is required to build `rvcsi-adapter-nexmon` (already true for many workspace crates via transitive `cc` deps; acceptable).
- The "rvCSI Nexmon record" is a *normalized* format, not byte-identical to any upstream nexmon_csi build — a thin demux/transcode step is needed when wiring real Nexmon output. This is intentional (we control the contract the shim parses) and documented.
- JSONL captures are larger than a packed binary format; fine for v0 (and the PRD already standardizes on JSON/WebSocket on the wire), revisit if capture size becomes a problem.
- `rvcsi-node` as a workspace member adds the `napi` dependency tree to `cargo build --workspace`; mitigated by it being a small, well-maintained crate.

**Risks**

- napi-rs major-version churn could change the macro/`build.rs` surface; pinned to `napi = "2.16"` in workspace deps, bumped deliberately.
- If a future platform can't link a napi cdylib under plain `cargo build`, `rvcsi-node` moves to the workspace `exclude` list (like `wifi-densepose-wasm-edge`) with a separate build command — same pattern, already established.

---

## 4. Alternatives considered

| Alternative | Why not |
|-------------|---------|
| One mega-crate `rvcsi` instead of eight | Couples DSP/events/adapters/FFI; can't review or implement them independently; bloats compile units for downstream users who only want `rvcsi-core`. |
| `bindgen` for the C shim | Pulls in `libclang`; the shim's C API is six functions — hand-written `extern "C"` decls are clearer and dependency-free. |
| Binary `.rvcsi` capture format (bincode/custom) | Smaller, but not human-inspectable; JSONL is debuggable, append-friendly, and matches the PRD's on-the-wire JSON. Revisit if size matters. |
| Expose raw `CsiFrame` pointers / typed arrays across napi for zero-copy | Violates ADR-095 D6 (validate-before-FFI) and the "no raw pointers to TS" safety NFR; the per-frame copy cost is negligible at the target rates. |
| `wasm-bindgen` instead of napi-rs for the JS surface | WASM can't do live capture (no raw sockets/serial); great for offline parsing (a later target) but not the primary Node runtime. |
| `rvcsi-events` depending on `rvcsi-dsp` for window stats | Adds a coordination point for two leaf crates; the stats are a few lines — keep the leaves independent and let higher layers compose them. |

---

## 5. Status of the implementation (this PR)

- `rvcsi-core` — implemented, `forbid(unsafe_code)`, 29 unit tests.
- `rvcsi-adapter-nexmon` + the napi-c shim — implemented; C compiled via `build.rs`+`cc`; `ffi` wrappers + `NexmonAdapter`; 9 tests round-tripping through the C shim.
- `rvcsi-dsp`, `rvcsi-events`, `rvcsi-adapter-file`, `rvcsi-ruvector` — implemented (parallel swarm), each with its own test suite.
- `rvcsi-node` (napi-rs surface) and `rvcsi-cli` — implemented (the addon's Rust surface + the `rvcsi` subcommands); the `@ruv/rvcsi` npm wrapper and a Node smoke test ship alongside.
- `rvcsi-mcp` (MCP tool server) and `rvcsi-daemon` (long-running capture service) — not in this PR; tracked as follow-ups on top of `rvcsi-node`.

---

## 6. References

- [ADR-095 — rvCSI Edge RF Sensing Platform](ADR-095-rvcsi-edge-rf-sensing-platform.md)
- [rvCSI Platform PRD](../prd/rvcsi-platform-prd.md)
- [rvCSI Domain Model](../ddd/rvcsi-domain-model.md)
- napi-rs — https://napi.rs/
- nexmon_csi — the upstream Broadcom CSI extractor the record format normalizes
