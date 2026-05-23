# ADR-116: Home Assistant + Matter as a Cognitum Seed cog (`cog-ha-matter`)

| Field | Value |
|-------|-------|
| **Status** | Proposed (research in progress — `docs/research/ADR-116-ha-matter-cog-research.md`) |
| **Date** | 2026-05-23 |
| **Deciders** | ruv |
| **Codename** | **HA-COG** — HA + Matter, packaged for the Seed |
| **Relates to** | [ADR-110](ADR-110-esp32-c6-firmware-extension.md) (C6 firmware substrate), [ADR-115](ADR-115-home-assistant-integration.md) (HA-DISCO + HA-MIND + HA-FABRIC), [ADR-102](ADR-102-edge-module-registry.md) (cog catalog), [ADR-101](ADR-101-pose-estimation-cog.md) (cog packaging precedent) |
| **Tracking issue** | TBD — file under RuView issue tracker once research dossier lands |

---

## 1. Context

ADR-115 shipped the Home Assistant + Matter integration as a **`--mqtt` flag on `wifi-densepose-sensing-server`** — a Rust binary that runs on a Pi / Linux box, consumes UDP frames from the ESP32 fleet, and publishes MQTT for any Home Assistant install to discover. That works, but it makes HA+Matter a *configuration of the aggregator*, not an *installable artifact* a Cognitum Seed user can drop into their existing fleet.

The Cognitum Seed already has a [105-cog catalog](https://seed.cognitum.one/store) — packaged Seed apps (`cog-pose-estimation`, `cog-quantum-vitals`, `cog-person-matching`, etc.) that anyone can install from `app-registry.json`. **There is no `cog-ha-matter` yet.** That's the gap this ADR closes.

The cog packaging precedent is ADR-101 (`cog-pose-estimation`) which ships signed aarch64 + x86_64 binaries on GCS with a `pose_v1.safetensors` weight blob — same shape we'd want for the HA cog.

### 1.1 Why a cog, not just the existing flag?

| Path | Distribution | Discovery | Update | Witness | Local AI |
|---|---|---|---|---|---|
| `--mqtt` on `sensing-server` | manual install of the Rust binary | none | manual | none | external |
| **`cog-ha-matter` Seed cog** | `app-registry.json` listing, one-click install | mDNS / cog browser | OTA via cog runtime | Ed25519 witness chain | local ruvllm + RuVector |

The cog ships HA+Matter as a first-class Seed feature — same UX as installing a pose estimator or person matcher.

### 1.2 What this ADR is *not*

- Not a deprecation of the `--mqtt` flag on sensing-server. The flag stays for Pi / Linux deployments without a Seed; the cog is the Seed-native option.
- Not a port of HA-MIND / HA-DISCO logic to a different language. The Rust crate already exists; the cog *wraps* it as a Seed-installable artifact + adds Seed-specific surfaces (witness, RuVector, ruvllm-driven thresholds).
- Not a Matter SDK ship. ADR-115 §9.10 deferred the matter-rs SDK wiring to v0.7.1; this ADR continues that deferral and focuses on the *cog packaging* + *first-class Seed integration*, with Matter Bridge mode shipping in v0.8 once the SDK is ready.

## 2. Decision (provisional — to be refined by the research dossier)

Build **`cog-ha-matter`** as a Cognitum Seed cog with these surfaces:

### 2.1 Core entity surface (unchanged from ADR-115)

The cog republishes the same 21 entities per node (11 raw + 10 semantic primitives) over MQTT auto-discovery, so HA installations behave identically whether the source is a Seed cog or an external sensing-server.

### 2.2 Seed-native enhancements

- **Self-contained MQTT broker (optional)** — if the user doesn't already run mosquitto, the cog can host an embedded broker on `cognitum-seed.local:1883` and act as the HA endpoint directly.
- **mDNS service advertisement** — `_ruview-ha._tcp` so HA's discovery integration finds the Seed without manual config.
- **RuVector-backed semantic-primitive thresholds** — instead of static `semantic-thresholds.yaml`, the cog learns per-home thresholds via a SONA-adapted RuVector model (matches the Seed's local-first AI story).
- **Ed25519 witness chain** — every state transition logged with a Seed signature so care-home / regulated deployments can audit decisions.
- **OTA firmware coordination** — the cog manages C6 firmware updates for ESP32-C6 nodes in the mesh (ADR-110 substrate).

### 2.3 Matter dimensions (depend on research findings)

The research dossier covers (a) Matter Bridge vs Matter Device mode, (b) Thread Border Router on the Seed's ESP32-S3 (if feasible), (c) CSA certification path, (d) which Matter device classes map cleanly to which entities. **Decision deferred** until the dossier lands; this ADR will be updated in §3 with the specific Matter feature set.

### 2.4 Multi-Seed federation

Multiple Seeds in adjacent rooms coordinate via:
- ESP-NOW mesh (ADR-110 substrate) for time alignment
- mDNS for service discovery
- Witness chain replication for cross-Seed event provenance

The federation model is the natural extension of ADR-110's mesh substrate into the application layer. Specifically: ADR-110 gives us ≤100 µs cross-board sync; this ADR uses that to deduplicate cross-Seed events (one fall, one alert) and reconstruct multi-room transitions (one occupant, room A → hallway → room B).

## 3. Open questions (research dossier will answer)

1. **Matter Bridge vs Matter Root** — can the Seed act as a commissioner, or is Bridge mode the only realistic 2026 option?
2. **Thread Border Router** — does the ESP32-S3 in the Seed (or a paired ESP32-C6 node) host a Thread Border Router cleanly, and does that buy us anything HA users care about?
3. **HACS integration value-add** — beyond MQTT auto-discovery, what does a custom HA integration unlock? (config flow / repairs / device-class custom entities / service catalog).
4. **CSA certification cost / timeline** — what's the minimum CSA-compliant subset and what does it cost to ship a CSA-certified Matter device today?
5. **Cog binary size + Seed RAM budget** — the Seed has 8 MB PSRAM + 320 KB SRAM. The cog must fit.
6. **ruvllm + RuVector latency for semantic primitives** — can the 10 inferred states run at 5 Hz on the Seed without external compute?
7. **HIPAA / FDA classification** — when does fall-detection cross into regulated medical-device territory, and does the `--privacy-mode` strip + Matter Health device class give us a defensible "wellness device" position?

## 4. Implementation phases

| Phase | Scope | Status |
|---|---|---|
| **P1** | Research dossier (`docs/research/ADR-116-ha-matter-cog-research.md`) | _in progress_ (deep-researcher agent) |
| **P2** | Cog manifest + binary scaffold (`cogs/ha-matter/`) | pending |
| **P3** | Wrap existing ADR-115 MQTT publisher as cog entry point | pending |
| **P4** | Seed-native enhancements (embedded broker, mDNS, witness) | pending |
| **P5** | RuVector-backed threshold learning (SONA adaptation) | pending |
| **P6** | Multi-Seed federation (cross-Seed dedup + witness) | pending |
| **P7** | Matter Bridge mode (depends on matter-rs / esp-matter readiness) | pending |
| **P8** | Cog signing + `app-registry.json` listing + Seed Store entry | pending |
| **P9** | HACS integration repo (`hass-wifi-densepose`) for HA-side install path | pending |
| **P10** | Witness bundle + CSA-style spec compliance check | pending |

## 5. References

- ADR-101 — `cog-pose-estimation` packaging precedent (signed binaries on GCS, .cog manifest)
- ADR-102 — edge module registry (`app-registry.json` surfaces all cogs)
- ADR-110 — ESP32-C6 firmware substrate (mesh time alignment that multi-Seed federation depends on)
- ADR-115 — HA-DISCO + HA-MIND + HA-FABRIC (the Rust crate this cog wraps)
- `docs/research/ADR-116-ha-matter-cog-research.md` — companion research dossier (deep-researcher agent in progress)
- Cognitum Seed store: https://seed.cognitum.one/store
- Matter spec: https://csa-iot.org/all-solutions/matter/
- HACS integration target: https://github.com/ruvnet/hass-wifi-densepose (planned)
