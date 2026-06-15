# Through-Wall WiFi Sensing Demo (LIVE CSI — no simulation, no fake skeleton)

A self-contained 3D demo that renders **only real data** streamed from the
running `wifi-densepose-sensing-server`, which ingests genuine WiFi Channel
State Information (CSI) from a live ESP32-S3 node over UDP.

It honestly shows what WiFi CSI sensing actually delivers:

- **motion** and **presence** — does the RF field say someone is here and moving?
- a **coarse RF localization** marker — roughly *where* the energy is, in metres.
- a **20×20 signal-field heatmap** on the floor — the live "where is the motion" map.

…and it shows all of this **through drywall**. That is the real wow of WiFi
sensing — not skeletal pose.

## What this is NOT

- **Not a skeleton / pose.** The sensing-server's `persons[].keypoints` carry
  `confidence: 0.0` (they are image-pixel placeholders, not real 3D joints), so
  this demo never draws them. WiFi CSI here gives motion / presence / coarse
  position — that is the honest output, and we render exactly that.
- **Not a simulation.** If the server is sending `source: "simulated"`, the
  banner says **SIMULATED — not real** in orange. If the server is unreachable,
  the page shows **NO SERVER** with start instructions. It never invents frames.

## What it renders (all driven by real `/ws/sensing` frames)

| Element | Real field used |
|---|---|
| Floor heatmap (20×20 tiles) | `signal_field.values` (400 floats ~0..1) |
| Coarse localization puck | `persons[0].position` `[x,0,z]` (peak cell as fallback) |
| Motion / breathing / variance / RSSI bars | `features.*` |
| Presence / motion level / confidence | `classification.*` |
| Estimated persons | `estimated_persons` |
| Active node markers | `nodes[].node_id` (node 9 = office, node 13 = hallway) |
| Update rate (Hz) | measured from frame arrival times |
| Status banner | `source` verbatim ("esp32" = LIVE) |

The 3D room is split by a **wall + doorway** into **OFFICE** (node 9) and
**HALLWAY** (node 13). Node markers light up only when that node actually
appears in the live `nodes` list.

## The through-wall story

WiFi (2.4/5 GHz) penetrates interior drywall. When you walk from the office
into the hallway — *behind the wall* — node 9's `signal_field` and
`motion_band_power` **still register the motion** even though there is a wall
between you and the antenna. That is real through-wall motion sensing on a
single node.

Once a **second ESP32-S3 is flashed and placed in the hallway** (node 13, the
`esp32-csi-node` firmware), the server fuses both nodes (multistatic) and the
hallway node localizes you on its side of the wall — true two-room through-wall
localization. With one node today you already get through-wall *motion*; the
second node adds *where*.

## How to run

### 1. Start the REAL sensing-server

```bash
cd v2
cargo build -p wifi-densepose-sensing-server
./target/debug/sensing-server.exe --ws-port 8765 --udp-port 5005
```

This is the process that ingests real CSI from the ESP32-S3 (UDP on 5005) and
serves the live WebSocket on `ws://localhost:8765/ws/sensing`. A real ESP32-S3
must be provisioned and streaming for `source` to read `esp32` (see the repo's
ESP32 firmware build/provision steps in `CLAUDE.local.md`).

### 2. Start the static server for this page

```bash
python examples/through-wall/serve.py
```

(Serves on **port 8080** — 8765 is the WebSocket, a different process.)

### 3. Open the page

```
http://localhost:8080/examples/through-wall/index.html
```

The page connects automatically. If you want to point at a server on another
host (e.g. an ESP32 streaming to a Pi), override the endpoint:

```
http://localhost:8080/examples/through-wall/index.html?ws=ws://192.168.1.20:8765/ws/sensing
```

## Optional: webcam ground-truth tile

The bottom-right tile can enable your webcam ("camera — ground truth when
visible"). This is **separate** from the CSI sensing — it is only there to let a
viewer confirm with their eyes what the WiFi is detecting. The WiFi works in the
dark and through walls; the camera does not. The sensing itself is the CSI.

## Honest scope

- Real: motion, presence, coarse position (incl. through drywall on the office
  node), the live signal-field heatmap, RSSI, and a measured update rate.
- The coarse-localization puck is labeled **"RF localization (coarse)"** — it is
  metre-scale, not centimetre pose. It uses `persons[0].position` when a person
  is tracked, otherwise the peak cell of the live `signal_field`.
- Two-room (office + hallway) through-wall *localization* needs the hallway node
  (node 13) flashed and placed; until then node 13 stays dimmed and the demo
  shows single-node through-wall *motion*.

## Reused from existing examples

- 3D scene setup, lights, fog, post-processing bloom, dark amber CSS, and the
  optional webcam path — from `examples/three.js/demos/05-skinned-realtime.html`.
- Floor-heatmap-on-the-grid idea and presence/field rendering — from
  `ui/observatory/js/` (`presence-cartography.js`, `subcarrier-manifold.js`).
- Threaded no-cache static server — from
  `examples/three.js/server/serve-demo.py`.
