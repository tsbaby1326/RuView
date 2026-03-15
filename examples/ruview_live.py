#!/usr/bin/env python3
"""
RuView Live — Unified Real-Time Ambient Intelligence Dashboard

Combines all available RuView sensors into a single live display:
  - ESP32-S3 WiFi CSI (serial or UDP): presence, motion, breathing, heart rate
  - MR60BHA2 mmWave (serial): precise HR, BR, presence, distance, light
  - Derived: blood pressure, stress (HRV), sleep state, activity

Automatically detects which sensors are available and adapts.

Usage:
    python examples/ruview_live.py
    python examples/ruview_live.py --csi COM7 --mmwave COM4
    python examples/ruview_live.py --csi COM7  # CSI only
    python examples/ruview_live.py --mmwave COM4  # mmWave only
"""

import argparse
import collections
import math
import re
import serial
import sys
import threading
import time

# ---- Regex patterns ----
RE_ANSI = re.compile(r"\x1b\[[0-9;]*m")
# mmWave (ESPHome)
RE_MW_HR = re.compile(r"'Real-time heart rate'.*?(\d+\.?\d*)\s*bpm", re.I)
RE_MW_BR = re.compile(r"'Real-time respiratory rate'.*?(\d+\.?\d*)", re.I)
RE_MW_PRES = re.compile(r"'Person Information'.*?state\s+(ON|OFF)", re.I)
RE_MW_DIST = re.compile(r"'Distance to detection object'.*?(\d+\.?\d*)\s*cm", re.I)
RE_MW_LUX = re.compile(r"illuminance=(\d+\.?\d*)", re.I)
RE_MW_TARGETS = re.compile(r"'Target Number'.*?(\d+\.?\d*)", re.I)
# CSI (edge_proc)
RE_CSI_VITALS = re.compile(r"Vitals:.*?br=(\d+\.?\d*).*?hr=(\d+\.?\d*).*?motion=(\d+\.?\d*).*?pres=(\w+)", re.I)
RE_CSI_CB = re.compile(r"CSI cb #(\d+).*?rssi=(-?\d+)")
RE_CSI_CALIB = re.compile(r"Adaptive calibration.*?threshold=(\d+\.?\d*)")
RE_CSI_FALL = re.compile(r"Fall detected.*?accel=(\d+\.?\d*)")


class SensorHub:
    """Aggregates data from all sensors with thread-safe access."""

    def __init__(self):
        self.lock = threading.Lock()
        # mmWave
        self.mw_hr = 0.0
        self.mw_br = 0.0
        self.mw_presence = False
        self.mw_distance = 0.0
        self.mw_lux = 0.0
        self.mw_targets = 0
        self.mw_frames = 0
        self.mw_connected = False
        # CSI
        self.csi_hr = 0.0
        self.csi_br = 0.0
        self.csi_motion = 0.0
        self.csi_presence = False
        self.csi_rssi = 0
        self.csi_frames = 0
        self.csi_calibrated = False
        self.csi_calib_thresh = 0.0
        self.csi_fall = False
        self.csi_connected = False
        # Derived
        self.hr_history = collections.deque(maxlen=120)
        self.events = collections.deque(maxlen=50)

    def update_mw(self, **kw):
        with self.lock:
            for k, v in kw.items():
                setattr(self, f"mw_{k}", v)
            self.mw_connected = True

    def update_csi(self, **kw):
        with self.lock:
            for k, v in kw.items():
                setattr(self, f"csi_{k}", v)
            self.csi_connected = True

    def add_hr(self, hr):
        if 30 < hr < 200:
            self.hr_history.append(hr)

    def add_event(self, msg):
        self.events.append((time.time(), msg))

    def snapshot(self):
        with self.lock:
            return {k: getattr(self, k) for k in vars(self) if not k.startswith("_") and k != "lock"}


def compute_derived(hub_snap):
    """Compute fused vitals + derived metrics."""
    d = {}

    # Fused HR: prefer mmWave, fallback CSI
    mw_hr = hub_snap["mw_hr"]
    csi_hr = hub_snap["csi_hr"]
    if mw_hr > 0 and csi_hr > 0:
        d["hr"] = mw_hr * 0.8 + csi_hr * 0.2
        d["hr_src"] = "Fused"
    elif mw_hr > 0:
        d["hr"] = mw_hr
        d["hr_src"] = "mmWave"
    elif csi_hr > 0:
        d["hr"] = csi_hr
        d["hr_src"] = "CSI"
    else:
        d["hr"] = 0
        d["hr_src"] = "—"

    # Fused BR
    mw_br = hub_snap["mw_br"]
    csi_br = hub_snap["csi_br"]
    if mw_br > 0 and csi_br > 0:
        d["br"] = mw_br * 0.8 + csi_br * 0.2
    elif mw_br > 0:
        d["br"] = mw_br
    elif csi_br > 0:
        d["br"] = csi_br
    else:
        d["br"] = 0

    # Fused presence (OR)
    d["presence"] = hub_snap["mw_presence"] or hub_snap["csi_presence"]

    # HRV from HR history
    hrs = list(hub_snap["hr_history"])
    if len(hrs) >= 5:
        rr = [60000.0 / h for h in hrs if h > 0]
        rr_mean = sum(rr) / len(rr)
        d["sdnn"] = math.sqrt(sum((x - rr_mean) ** 2 for x in rr) / len(rr))
        diffs = [(rr[i + 1] - rr[i]) ** 2 for i in range(len(rr) - 1)]
        d["rmssd"] = math.sqrt(sum(diffs) / len(diffs)) if diffs else 0
    else:
        d["sdnn"] = 0
        d["rmssd"] = 0

    # Blood pressure estimate
    if d["hr"] > 0 and d["sdnn"] > 0:
        delta = d["hr"] - 72
        d["sbp"] = round(max(80, min(200, 120 + 0.5 * delta - 0.8 * (d["sdnn"] - 50) / 50)))
        d["dbp"] = round(max(50, min(130, 80 + 0.3 * delta - 0.5 * (d["sdnn"] - 50) / 50)))
    else:
        d["sbp"] = 0
        d["dbp"] = 0

    # Stress level
    if d["sdnn"] > 0:
        if d["sdnn"] < 30:
            d["stress"] = "HIGH"
        elif d["sdnn"] < 50:
            d["stress"] = "Moderate"
        elif d["sdnn"] < 80:
            d["stress"] = "Mild"
        elif d["sdnn"] < 100:
            d["stress"] = "Relaxed"
        else:
            d["stress"] = "Calm"
    else:
        d["stress"] = "—"

    # Light
    d["lux"] = hub_snap["mw_lux"]
    if d["lux"] < 1:
        d["light"] = "Dark"
    elif d["lux"] < 10:
        d["light"] = "Dim"
    elif d["lux"] < 50:
        d["light"] = "Low"
    elif d["lux"] < 200:
        d["light"] = "Normal"
    else:
        d["light"] = "Bright"

    return d


def reader_mmwave(port, baud, hub, stop):
    try:
        ser = serial.Serial(port, baud, timeout=1)
        hub.add_event(f"mmWave connected on {port}")
    except Exception as e:
        hub.add_event(f"mmWave FAILED: {e}")
        return

    prev_pres = None
    while not stop.is_set():
        try:
            line = ser.readline().decode("utf-8", errors="replace")
        except Exception:
            continue
        clean = RE_ANSI.sub("", line)

        m = RE_MW_HR.search(clean)
        if m:
            hr = float(m.group(1))
            hub.update_mw(hr=hr, frames=hub.mw_frames + 1)
            hub.add_hr(hr)

        m = RE_MW_BR.search(clean)
        if m:
            hub.update_mw(br=float(m.group(1)))

        m = RE_MW_PRES.search(clean)
        if m:
            pres = m.group(1) == "ON"
            if prev_pres is not None and pres != prev_pres:
                hub.add_event(f"mmWave: person {'arrived' if pres else 'left'}")
            prev_pres = pres
            hub.update_mw(presence=pres)

        m = RE_MW_DIST.search(clean)
        if m:
            hub.update_mw(distance=float(m.group(1)))

        m = RE_MW_LUX.search(clean)
        if m:
            hub.update_mw(lux=float(m.group(1)))

        m = RE_MW_TARGETS.search(clean)
        if m:
            hub.update_mw(targets=int(float(m.group(1))))

    ser.close()


def reader_csi(port, baud, hub, stop):
    try:
        ser = serial.Serial(port, baud, timeout=1)
        hub.add_event(f"CSI connected on {port}")
    except Exception as e:
        hub.add_event(f"CSI FAILED: {e}")
        return

    while not stop.is_set():
        try:
            line = ser.readline().decode("utf-8", errors="replace")
        except Exception:
            continue

        m = RE_CSI_VITALS.search(line)
        if m:
            hub.update_csi(
                br=float(m.group(1)),
                hr=float(m.group(2)),
                motion=float(m.group(3)),
                presence=(m.group(4).upper() == "YES"),
            )
            hub.add_hr(float(m.group(2)))

        m = RE_CSI_CB.search(line)
        if m:
            hub.update_csi(frames=int(m.group(1)), rssi=int(m.group(2)))

        m = RE_CSI_CALIB.search(line)
        if m:
            hub.update_csi(calibrated=True, calib_thresh=float(m.group(1)))
            hub.add_event(f"CSI calibrated (threshold={m.group(1)})")

        m = RE_CSI_FALL.search(line)
        if m:
            hub.update_csi(fall=True)
            hub.add_event(f"FALL DETECTED (accel={m.group(1)})")

    ser.close()


def display(hub, duration, interval=3):
    start = time.time()
    last = 0

    # Header
    print()
    print("=" * 78)
    print("  RuView Live — Ambient Intelligence Dashboard")
    print("=" * 78)
    print()
    cols = f"{'Time':>5} {'HR':>4} {'BR':>3} {'BP':>7} {'Stress':>8} {'SDNN':>5} " \
           f"{'Pres':>4} {'Dist':>5} {'Lux':>5} {'RSSI':>5} {'CSI#':>5} {'MW#':>4}"
    print(cols)
    print("-" * 78)

    while time.time() - start < duration:
        time.sleep(0.5)
        elapsed = int(time.time() - start)
        if elapsed <= last or elapsed % interval != 0:
            continue
        last = elapsed

        snap = hub.snapshot()
        d = compute_derived(snap)

        # Format
        hr_s = f"{d['hr']:>4.0f}" if d["hr"] > 0 else "  —"
        br_s = f"{d['br']:>3.0f}" if d["br"] > 0 else " —"
        bp_s = f"{d['sbp']:>3}/{d['dbp']:<3}" if d["sbp"] > 0 else "  —/—  "
        pres_s = "YES" if d["presence"] else " no"
        dist_s = f"{snap['mw_distance']:>4.0f}cm" if snap["mw_distance"] > 0 else "   — "
        lux_s = f"{d['lux']:>5.1f}" if d["lux"] > 0 else "  — "
        rssi_s = f"{snap['csi_rssi']:>5}" if snap["csi_rssi"] != 0 else "  — "

        print(f"{elapsed:>4}s {hr_s} {br_s} {bp_s} {d['stress']:>8} {d['sdnn']:>5.0f} "
              f"{pres_s:>4} {dist_s} {lux_s} {rssi_s} {snap['csi_frames']:>5} {snap['mw_frames']:>4}")

        # Print recent events
        for ts, msg in snap["events"]:
            age = elapsed - int(ts - (time.time() - elapsed))
            if 0 <= age < interval + 1:
                print(f"       >> {msg}")

    # Summary
    snap = hub.snapshot()
    d = compute_derived(snap)
    print()
    print("=" * 78)
    print("  SESSION SUMMARY")
    print("=" * 78)
    sensors = []
    if snap["csi_connected"]:
        sensors.append(f"CSI ({snap['csi_frames']} frames)")
    if snap["mw_connected"]:
        sensors.append(f"mmWave ({snap['mw_frames']} readings)")
    print(f"  Sensors:     {', '.join(sensors) if sensors else 'None detected'}")
    print(f"  Duration:    {duration}s")
    if d["hr"] > 0:
        print(f"  Heart Rate:  {d['hr']:.0f} bpm ({d['hr_src']})")
    if d["br"] > 0:
        print(f"  Breathing:   {d['br']:.0f}/min")
    if d["sbp"] > 0:
        print(f"  BP Estimate: {d['sbp']}/{d['dbp']} mmHg")
    if d["sdnn"] > 0:
        print(f"  HRV (SDNN):  {d['sdnn']:.0f} ms — {d['stress']}")
    if d["lux"] > 0:
        print(f"  Light:       {d['lux']:.1f} lux ({d['light']})")
    if snap["csi_rssi"] != 0:
        print(f"  WiFi RSSI:   {snap['csi_rssi']} dBm")
    events = list(snap["events"])
    if events:
        print(f"  Events ({len(events)}):")
        for ts, msg in events[-10:]:
            print(f"    {msg}")
    print()


def main():
    parser = argparse.ArgumentParser(description="RuView Live Dashboard")
    parser.add_argument("--csi", default="COM7", help="CSI serial port (or 'none')")
    parser.add_argument("--mmwave", default="COM4", help="mmWave serial port (or 'none')")
    parser.add_argument("--duration", type=int, default=120)
    parser.add_argument("--interval", type=int, default=3, help="Display update interval (seconds)")
    args = parser.parse_args()

    hub = SensorHub()
    stop = threading.Event()
    threads = []

    if args.mmwave.lower() != "none":
        t = threading.Thread(target=reader_mmwave, args=(args.mmwave, 115200, hub, stop), daemon=True)
        t.start()
        threads.append(t)

    if args.csi.lower() != "none":
        t = threading.Thread(target=reader_csi, args=(args.csi, 115200, hub, stop), daemon=True)
        t.start()
        threads.append(t)

    time.sleep(2)  # Let sensors connect

    try:
        display(hub, args.duration, args.interval)
    except KeyboardInterrupt:
        print("\nStopping...")

    stop.set()
    for t in threads:
        t.join(timeout=2)


if __name__ == "__main__":
    main()
