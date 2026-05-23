# Home Assistant integration

RuView publishes its full WiFi-sensing capability set to **Home Assistant** via MQTT auto-discovery (HA-DISCO) and to **any Matter controller** (Apple Home / Google Home / Alexa / SmartThings / HA) via a built-in Matter Bridge (HA-FABRIC). This document is the operator guide for both paths. Design rationale: [ADR-115](../adr/ADR-115-home-assistant-integration.md).

> **Tested against** Home Assistant Core **2025.5**, Mosquitto add-on **6.4**, and Matter (chip-tool) **1.3**. Bump the matrix when you change tested versions.

---

## Quick start

### 1. Prereqs

- A running **MQTT broker** on your LAN. The easiest path is the [Mosquitto add-on](https://github.com/home-assistant/addons/tree/master/mosquitto) inside Home Assistant OS (one click from the Add-on Store). EMQX and VerneMQ also work — see §Advanced brokers below.
- Home Assistant **2025.5 or newer** with the MQTT integration enabled and pointed at your broker.
- A RuView **`wifi-densepose-sensing-server`** v0.7.0+ binary (or `cargo run` from source).

### 2. Start the publisher

```bash
# Docker (recommended for non-developers):
docker run --rm --net=host \
    ruvnet/wifi-densepose:0.7.0 \
    --source esp32 \
    --mqtt --mqtt-host 192.168.1.10 \
    --mqtt-username homeassistant --mqtt-password-env MQTT_PASSWORD

# Or from a source checkout (Rust 1.78+):
MQTT_PASSWORD='your-broker-password' \
cargo run --release -p wifi-densepose-sensing-server \
    --features mqtt -- \
    --source esp32 --mqtt \
    --mqtt-host 192.168.1.10 \
    --mqtt-username homeassistant
```

Within ~5 seconds of starting, Home Assistant should auto-create:

- One **device** per RuView node (named after the MAC or the `friendly_name` from your zones config)
- 17+ **entities** per device (presence, person count, heart rate, breathing rate, motion, fall events, signal strength, zones, and the 10 semantic primitives)

If nothing appears in HA's Settings → Devices, see [Troubleshooting](#troubleshooting).

### 3. Stop the publisher cleanly

Ctrl-C — the publisher pushes `offline` to every availability topic before disconnect so HA marks all entities unavailable instantly. A `kill -9` triggers MQTT LWT, which has the same effect within ~30 s.

---

## Entity reference

RuView publishes three classes of entity. Names below are the `unique_id` slugs — Home Assistant assigns friendly names automatically.

### Raw signals (11 entities)

| HA entity | Slug | HA component | Unit | Source field |
|---|---|---|---|---|
| Presence | `presence` | `binary_sensor` | — | `edge_vitals.presence` |
| Person count | `person_count` | `sensor` | persons | `edge_vitals.n_persons` |
| Heart rate | `heart_rate` | `sensor` | bpm | `edge_vitals.heartrate_bpm` |
| Breathing rate | `breathing_rate` | `sensor` | bpm | `edge_vitals.breathing_rate_bpm` |
| Motion level | `motion_level` | `sensor` | % | `edge_vitals.motion` × 100 |
| Motion energy | `motion_energy` | `sensor` | (dimensionless) | `edge_vitals.motion_energy` |
| Fall detected | `fall` | `event` | — | `edge_vitals.fall_detected` |
| Presence score | `presence_score` | `sensor` | % | `edge_vitals.presence_score` × 100 |
| Signal strength | `rssi` | `sensor` | dBm | `edge_vitals.rssi` |
| Zone occupancy | `zone_occupancy` | `binary_sensor` | — | `sensing_update.zones` |
| Pose keypoints | `pose` | `sensor` (attrs) | — | `pose_data.keypoints` (opt-in via `--mqtt-publish-pose`) |

Heart rate, breathing rate, and pose are **biometric** entities — they are stripped from MQTT (and never published over Matter) when `--privacy-mode` is set. See [Privacy](#privacy) below.

### Semantic automation primitives (10 entities)

These are the inferred high-level states that customer automations actually use. Each one is a small finite-state machine running server-side with explicit warmup, hysteresis, and refractory windows. Per-primitive precision/recall is published in [`semantic-primitives-metrics.md`](./semantic-primitives-metrics.md).

| HA entity | Slug | HA component | What it fires on |
|---|---|---|---|
| Someone sleeping | `someone_sleeping` | `binary_sensor` | presence + motion<5% + BR ∈ [8,20] bpm sustained for 5 min |
| Possible distress | `possible_distress` | `binary_sensor` | HR > 1.5× baseline + motion >20% + no fall, sustained 60 s |
| Room active | `room_active` | `binary_sensor` | motion >10% in a 30-s rolling window |
| Elderly inactivity anomaly | `elderly_inactivity_anomaly` | `binary_sensor` | idle > 2× observed-max-idle baseline |
| Meeting in progress | `meeting_in_progress` | `binary_sensor` | ≥2 persons + low-amplitude motion for 10 min |
| Bathroom occupied | `bathroom_occupied` | `binary_sensor` | presence + active zone tagged `bathroom` |
| Fall risk elevated | `fall_risk_elevated` | `sensor` | 0–100 score; event fires on ≥70 crossing |
| Bed exit (overnight) | `bed_exit` | `event` | sleeping → presence leaves bed zone between 22:00–06:00 |
| No movement (safety) | `no_movement` | `binary_sensor` | presence + motion <1% for 30 min |
| Multi-room transition | `multi_room_transition` | `event` | zone X exit + zone Y enter within 10 s |

Every state change carries a `reason` attribute (e.g. `["motion<5%", "br=12bpm", "presence=true"]`) so you can template against it in HA automations to understand why an automation triggered.

### Matter device-type mapping

Per ADR-115 §3.11.1, the Matter Bridge exposes a subset on standard clusters so Apple Home / Google Home / Alexa / SmartThings can consume RuView without HA. Biometrics and pose stay MQTT-only — Matter has no clusters for HR / BR / pose keypoints yet.

| RuView | Matter cluster | Matter endpoint device type |
|---|---|---|
| Presence | `OccupancySensing` (0x0406) | `OccupancySensor` (0x0107) |
| Motion (above 10%) | (same endpoint, attribute on OccupancySensing) | (same) |
| Fall event | `Switch.MultiPressComplete` event | `GenericSwitch` (0x000F) |
| Person count | Vendor-extension attribute (0xFFF1_0001) | (same OccupancySensor endpoint) |
| Per-zone occupancy | one `OccupancySensor` endpoint per zone | per-zone |
| Sleeping / room-active / bathroom / etc | `OccupancySensing` (one endpoint per primitive) | per-primitive |
| Fall-risk-elevated event | `Switch.MultiPressComplete` event | `GenericSwitch` |
| HR / BR / pose | **not exposed** — MQTT only | — |

---

## Configuration

### CLI matrix

| Flag | Default | Purpose |
|---|---|---|
| `--mqtt` | off | Enable the HA-DISCO publisher |
| `--mqtt-host <HOST>` | `localhost` | Broker host |
| `--mqtt-port <PORT>` | 1883 (8883 with TLS) | Broker port |
| `--mqtt-username <U>` | — | Username for broker auth |
| `--mqtt-password-env <VAR>` | `MQTT_PASSWORD` | Env var holding the password |
| `--mqtt-client-id <ID>` | `wifi-densepose-<hostname>` | MQTT client ID |
| `--mqtt-prefix <PREFIX>` | `homeassistant` | Discovery topic prefix |
| `--mqtt-tls` | off | Encrypt connection |
| `--mqtt-ca-file <PATH>` | — | Pinned CA for TLS / mTLS |
| `--mqtt-client-cert <PATH>` | — | Client cert for mTLS |
| `--mqtt-client-key <PATH>` | — | Client key for mTLS |
| `--mqtt-refresh-secs <N>` | 600 | Discovery re-emit interval |
| `--mqtt-rate-vitals <HZ>` | 0.2 | HR / BR publish rate (Hz) |
| `--mqtt-rate-motion <HZ>` | 1.0 | Motion publish rate (Hz) |
| `--mqtt-rate-count <HZ>` | 1.0 | Person-count publish rate (Hz) |
| `--mqtt-rate-rssi <HZ>` | 0.1 | RSSI publish rate (Hz) |
| `--mqtt-publish-pose` | off | Enable pose-keypoint publication |
| `--mqtt-rate-pose <HZ>` | 1.0 | Pose publish rate when enabled |
| `--privacy-mode` | off | Strip HR/BR/pose from MQTT and Matter |
| `--matter` | off | Enable the HA-FABRIC Matter Bridge |
| `--matter-setup-file <PATH>` | — | Where to write the QR + manual code |
| `--matter-reset` | off | Wipe fabric credentials and re-commission |
| `--matter-vendor-id <VID>` | `0xFFF1` (dev) | CSA-assigned vendor ID |
| `--matter-product-id <PID>` | `0x8001` | Product ID |
| `--semantic` | on | Enable inference layer |
| `--semantic-thresholds-file <PATH>` | — | Per-primitive threshold overrides |
| `--semantic-zones-file <PATH>` | — | Zone-tag map (`bathroom`, `bedroom`, …) |
| `--no-semantic <PRIMITIVE>` | — | Disable a specific primitive (repeatable) |

### Zone tag file format

```yaml
# semantic-zones.yaml — passed to --semantic-zones-file
zones:
  bathroom: ["zone_3", "zone_7"]
  bedroom:  ["zone_1"]
  kitchen:  ["zone_2"]
  living:   ["zone_5"]
bed_zones: ["zone_1"]
```

### Threshold overrides

```yaml
# semantic-thresholds.yaml — passed to --semantic-thresholds-file
sleep_dwell_secs: 300
distress_hr_multiple: 1.5
room_active_motion_threshold: 0.10
elderly_anomaly_multiple: 2.0
meeting_min_persons: 2
no_movement_dwell_secs: 1800
fall_risk_event_threshold: 70.0
```

---

## Privacy

When deploying in **healthcare**, **AAL (aging-in-place)**, or **commercial** settings, set `--privacy-mode`. This:

- **Strips** heart rate, breathing rate, and pose keypoints from every outbound MQTT publication.
- **Suppresses discovery** for those entities entirely — HA never even sees they exist.
- **Keeps every semantic primitive enabled.** Sleeping / distress / room-active / etc are *inferred* states. The inference happens server-side and only the boolean or score crosses the wire. This is the architectural win that makes the platform deployable in regulated contexts.

Always pair `--privacy-mode` with `--mqtt-tls` on non-localhost brokers.

---

## Three starter blueprints

Drop these YAML files into `<HA config>/blueprints/automation/ruvnet/` and import them from the HA UI (Settings → Automations → Blueprints → Import).

### 1. Notify on possible distress

```yaml
blueprint:
  name: RuView — notify on possible distress
  description: >
    Send a push notification when RuView detects sustained elevated heart
    rate + agitated motion (possible distress).
  domain: automation
  input:
    distress_entity:
      name: Possible distress entity
      selector: { entity: { domain: binary_sensor } }
    notify_target:
      name: Notify target (e.g. notify.mobile_app_pixel)
      selector: { text: {} }

trigger:
  - platform: state
    entity_id: !input distress_entity
    to: "on"

action:
  - service: !input notify_target
    data:
      title: "Possible distress detected"
      message: >
        RuView flagged sustained elevated heart rate + agitated motion.
        Reason: {{ state_attr(trigger.entity_id, 'reason') }}.
```

### 2. Dim hallway when someone is sleeping

```yaml
blueprint:
  name: RuView — dim hallway when someone sleeping
  description: >
    Drop hallway lights to 10 % brightness when anyone in the bedroom is
    in the someone-sleeping state, so a midnight bathroom trip doesn't
    require full lights.
  domain: automation
  input:
    sleeping_entity:
      name: Someone sleeping entity
      selector: { entity: { domain: binary_sensor } }
    hallway_light:
      name: Hallway light
      selector: { entity: { domain: light } }

trigger:
  - platform: state
    entity_id: !input sleeping_entity
    to: "on"
  - platform: state
    entity_id: !input sleeping_entity
    to: "off"

action:
  - choose:
      - conditions:
          - condition: state
            entity_id: !input sleeping_entity
            state: "on"
        sequence:
          - service: light.turn_on
            target: { entity_id: !input hallway_light }
            data: { brightness_pct: 10 }
    default:
      - service: light.turn_off
        target: { entity_id: !input hallway_light }
```

### 3. Wake-up routine on bed exit

```yaml
blueprint:
  name: RuView — wake-up routine on bed exit
  description: >
    When bed_exit fires between 05:00 and 09:00, ramp up bedroom lights
    over 10 minutes, start the coffee maker, and disarm the home alarm.
  domain: automation
  input:
    bed_exit_event:
      name: Bed exit event entity
      selector: { entity: { domain: event } }
    bedroom_light:
      name: Bedroom light
      selector: { entity: { domain: light } }
    coffee_maker:
      name: Coffee maker switch
      selector: { entity: { domain: switch } }

trigger:
  - platform: state
    entity_id: !input bed_exit_event

condition:
  - condition: time
    after: "05:00:00"
    before: "09:00:00"

action:
  - service: light.turn_on
    target: { entity_id: !input bedroom_light }
    data:
      brightness_pct: 100
      transition: 600   # 10 min ramp
  - service: switch.turn_on
    target: { entity_id: !input coffee_maker }
  - service: alarm_control_panel.alarm_disarm
    target: { entity_id: alarm_control_panel.home }
```

---

## Lovelace dashboard examples

### Single-room overview card

```yaml
type: vertical-stack
title: Bedroom
cards:
  - type: glance
    entities:
      - entity: binary_sensor.ruview_bedroom_presence
      - entity: sensor.ruview_bedroom_heart_rate
      - entity: sensor.ruview_bedroom_breathing_rate
      - entity: sensor.ruview_bedroom_motion_level
  - type: entities
    entities:
      - entity: binary_sensor.ruview_bedroom_someone_sleeping
      - entity: binary_sensor.ruview_bedroom_room_active
      - entity: binary_sensor.ruview_bedroom_no_movement
      - entity: sensor.ruview_bedroom_fall_risk_elevated
```

### Multi-node grid

```yaml
type: grid
columns: 2
cards:
  - type: tile
    entity: binary_sensor.ruview_bedroom_presence
    name: Bedroom
  - type: tile
    entity: binary_sensor.ruview_living_presence
    name: Living
  - type: tile
    entity: binary_sensor.ruview_kitchen_presence
    name: Kitchen
  - type: tile
    entity: binary_sensor.ruview_bathroom_occupied
    name: Bathroom
```

---

## Advanced brokers

Mosquitto is the recommended default. The integration also works with:

- **EMQX** (https://www.emqx.io/) — clustering, MQTT 5.0, dashboard UI. Good for ≥10 RuView nodes.
- **VerneMQ** (https://vernemq.com/) — Erlang-based, multi-protocol bridges (AMQP, WebSocket).
- **HiveMQ Edge** (https://www.hivemq.com/edge/) — managed cloud relay if you need off-LAN access.

All three accept the same HA discovery topics RuView publishes. Performance and discovery semantics are identical.

---

## Troubleshooting

### No entities appear in HA

1. Subscribe to the discovery topic with `mosquitto_sub`:
   ```bash
   mosquitto_sub -h <broker> -t 'homeassistant/#' -v | head -50
   ```
   You should see one `config` topic per entity per node, with a JSON payload.
2. If `mosquitto_sub` shows nothing, RuView is not reaching the broker. Check `--mqtt-host`, network reachability, and credentials.
3. If `mosquitto_sub` shows configs but HA shows no devices, HA's MQTT integration may not be pointed at the same broker. Verify under Settings → Devices & Services → MQTT.

### Entities appear but state never updates

1. Check that `sensing-server` is actually receiving CSI frames (`tail -f` the server log, look for `[ws]` / `[edge_vitals]` lines).
2. Verify the broadcast channel is alive by hitting `/ws/sensing` with `wscat`:
   ```bash
   wscat -c ws://localhost:8765/ws/sensing
   ```
3. Confirm rate limits aren't dropping everything: `--mqtt-rate-vitals 1.0` for diagnosis (default 0.2 Hz = every 5 s).

### "Plaintext MQTT on non-localhost broker" WARN

Per [ADR-115 §3.9](../adr/ADR-115-home-assistant-integration.md#39-tls--auth), v0.7.0 warns and continues; v0.8.0 will hard-fail. Either:

- Add `--mqtt-tls` and supply a CA if your broker uses a self-signed cert, or
- Move the broker to `localhost` (e.g. run Mosquitto inside the same host as `sensing-server`).

### Matter pairing fails

1. Check the setup code in your `--matter-setup-file` log (defaults to printing on startup).
2. Make sure the host running `sensing-server` is on the same WiFi subnet as the controller.
3. If Apple Home complains about an unknown vendor, that's expected — RuView uses dev VID `0xFFF1` until P10 (see [ADR §9.9](../adr/ADR-115-home-assistant-integration.md#9b-matter-path-p7p10)). Tap "Add anyway".

---

## References

- [ADR-115](../adr/ADR-115-home-assistant-integration.md) — full design rationale
- [`semantic-primitives-metrics.md`](./semantic-primitives-metrics.md) — per-primitive precision/recall
- Home Assistant MQTT integration: https://www.home-assistant.io/integrations/mqtt/
- Mosquitto add-on: https://github.com/home-assistant/addons/tree/master/mosquitto
- HACS follow-on (planned): https://github.com/ruvnet/hass-wifi-densepose
- Matter spec: https://csa-iot.org/all-solutions/matter/
