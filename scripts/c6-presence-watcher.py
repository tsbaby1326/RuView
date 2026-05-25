#!/usr/bin/env python3
"""
c6-presence-watcher.py — ADR-125 iter 2.

Bridges real ESP32-C6 ADR-081 `rv_feature_state` UDP frames to the HAP
`MotionSensor` characteristic via the toggle file that
`scripts/hap-test-sensor.py` already pairs against. No mocks, no
simulation — consumes the exact 60-byte struct emitted by
`firmware/esp32-csi-node/main/rv_feature_state.[ch]`.

Wire format (RV_FEATURE_STATE_MAGIC = 0xC5110006, 60 bytes total,
__attribute__((packed))):

    offset  size  field            type
    0       4     magic            u32   = 0xC5110006
    4       1     node_id          u8
    5       1     mode             u8
    6       2     seq              u16
    8       8     ts_us            u64
    16      4     motion_score     f32   0..1, 100 ms window
    20      4     presence_score   f32   0..1, 1 s window
    24      4     respiration_bpm  f32
    28      4     respiration_conf f32
    32      4     heartbeat_bpm    f32
    36      4     heartbeat_conf   f32
    40      4     anomaly_score    f32
    44      4     env_shift_score  f32
    48      4     node_coherence   f32
    52      2     quality_flags    u16
    54      2     reserved         u16
    56      4     crc32            u32

`quality_flags & RV_QFLAG_PRESENCE_VALID (1<<0)` gates presence reads.
`presence_score >= PRESENCE_THRESHOLD` toggles motion ON; below the
release threshold (with hysteresis) toggles OFF. The toggle file
is the contract between this watcher and the paired HAP bridge.

Usage:
    python3 c6-presence-watcher.py [--port 5005] [--toggle /tmp/ruview-motion]
"""
from __future__ import annotations
import argparse
import os
import signal
import socket
import struct
import sys
import time
import zlib

RV_FEATURE_STATE_MAGIC = 0xC5110006
RV_QFLAG_PRESENCE_VALID = 1 << 0
PACKET_SIZE = 60

# Hysteresis — entry / exit thresholds keep the HomeKit characteristic
# from flapping when presence_score sits near the boundary.
PRESENCE_ON_THRESHOLD = 0.40
PRESENCE_OFF_THRESHOLD = 0.20
# Idle releases motion after this many seconds with no valid presence
# packets (covers the C6 falling off the air entirely).
IDLE_RELEASE_S = 5.0

# 60-byte packed layout (`<` = little-endian + no padding)
# magic|node|mode|seq|ts|motion|presence|resp_bpm|resp_c|hb_bpm|hb_c|anom|env|coh|qflags|reserved|crc
PACKET_STRUCT = struct.Struct("<IBBHQfffffffffHHI")
assert PACKET_STRUCT.size == PACKET_SIZE, (
    f"layout mismatch: struct {PACKET_STRUCT.size}, expected {PACKET_SIZE}"
)


def parse_packet(buf: bytes):
    """Return parsed dict or None if not a feature_state packet."""
    if len(buf) != PACKET_SIZE:
        return None
    fields = PACKET_STRUCT.unpack(buf)
    (magic, node_id, mode, seq, ts_us, motion, presence,
     resp_bpm, resp_conf, hb_bpm, hb_conf,
     anomaly, env_shift, coherence,
     qflags, _reserved, crc) = fields
    if magic != RV_FEATURE_STATE_MAGIC:
        return None
    # CRC32 over bytes [0..end-4]. Firmware uses IEEE poly == zlib.crc32.
    expected = zlib.crc32(buf[:-4]) & 0xFFFFFFFF
    crc_ok = expected == crc
    return {
        "node_id": node_id, "mode": mode, "seq": seq, "ts_us": ts_us,
        "motion": motion, "presence": presence,
        "resp_bpm": resp_bpm, "resp_conf": resp_conf,
        "hb_bpm": hb_bpm, "hb_conf": hb_conf,
        "anomaly": anomaly, "env_shift": env_shift, "coherence": coherence,
        "qflags": qflags, "crc_ok": crc_ok,
        "presence_valid": bool(qflags & RV_QFLAG_PRESENCE_VALID),
    }


def set_motion(toggle_file: str, on: bool, current: bool) -> bool:
    """Touch / unlink the toggle file iff state changes. Return new state."""
    if on == current:
        return current
    if on:
        with open(toggle_file, "w") as fh:
            fh.write("1\n")
    else:
        try:
            os.unlink(toggle_file)
        except FileNotFoundError:
            pass
    print(f"[{time.strftime('%H:%M:%S')}] motion -> {on}", flush=True)
    return on


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=5005)
    p.add_argument("--toggle", default="/tmp/ruview-motion")
    p.add_argument("--bind", default="0.0.0.0")
    args = p.parse_args()

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    if hasattr(socket, "SO_REUSEPORT"):
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
    sock.bind((args.bind, args.port))
    sock.settimeout(1.0)

    print(f"[c6-presence] listening udp {args.bind}:{args.port}", flush=True)
    print(f"[c6-presence] toggle file: {args.toggle}", flush=True)
    print(f"[c6-presence] thresholds: on>={PRESENCE_ON_THRESHOLD}, "
          f"off<={PRESENCE_OFF_THRESHOLD}, idle_release={IDLE_RELEASE_S}s",
          flush=True)

    running = True
    def _stop(*_):
        nonlocal running
        running = False
    signal.signal(signal.SIGTERM, _stop)
    signal.signal(signal.SIGINT, _stop)

    motion = os.path.exists(args.toggle)
    last_packet_ts = 0.0
    last_summary = time.time()
    n_total = n_valid = n_crc_bad = 0
    presence_sum = motion_sum = 0.0

    while running:
        try:
            buf, _addr = sock.recvfrom(2048)
        except socket.timeout:
            buf = None

        now = time.time()

        if buf is not None:
            n_total += 1
            pkt = parse_packet(buf)
            if pkt is not None:
                if not pkt["crc_ok"]:
                    n_crc_bad += 1
                elif pkt["presence_valid"]:
                    n_valid += 1
                    presence_sum += pkt["presence"]
                    motion_sum += pkt["motion"]
                    last_packet_ts = now
                    if not motion and pkt["presence"] >= PRESENCE_ON_THRESHOLD:
                        motion = set_motion(args.toggle, True, motion)
                    elif motion and pkt["presence"] <= PRESENCE_OFF_THRESHOLD:
                        motion = set_motion(args.toggle, False, motion)

        # Idle release — if the C6 stops sending entirely, clear motion.
        if motion and last_packet_ts and (now - last_packet_ts) > IDLE_RELEASE_S:
            motion = set_motion(args.toggle, False, motion)

        # Periodic summary line (every 10 s) so we can see the watcher is alive
        if now - last_summary >= 10.0:
            avg_p = presence_sum / n_valid if n_valid else 0.0
            avg_m = motion_sum / n_valid if n_valid else 0.0
            print(
                f"[{time.strftime('%H:%M:%S')}] 10s stats: "
                f"pkts={n_total} valid={n_valid} crc_bad={n_crc_bad} "
                f"avg_presence={avg_p:.2f} avg_motion={avg_m:.2f} motion={motion}",
                flush=True,
            )
            n_total = n_valid = n_crc_bad = 0
            presence_sum = motion_sum = 0.0
            last_summary = now

    sock.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
