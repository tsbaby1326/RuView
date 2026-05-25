#!/usr/bin/env python3
"""
hap-test-sensor.py — ADR-125 §2.1.a smoke test.

Stands up a single HomeKit Accessory Protocol (HAP-1.1) bridge with one
child MotionSensor named "RuView Test Motion". Once paired in the Apple
Home app, the HomePod (acting as Home Hub) sees state changes when
TOGGLE_FILE (default /tmp/ruview-motion) is touched / removed.

Usage:
    python3 hap-test-sensor.py

Pair from iPhone: Home app -> Add Accessory -> More Options -> "RuView Test Bridge".
The setup code is printed on stdout AND written to ~/.ruview-hap/setup-code.txt.

Trigger motion:  touch /tmp/ruview-motion
Clear motion:    rm   /tmp/ruview-motion

State persists across restarts in ~/.ruview-hap/accessory.state.
"""

from pathlib import Path
import os
import sys
import time
import signal

from pyhap.accessory import Accessory, Bridge
from pyhap.accessory_driver import AccessoryDriver
from pyhap.const import CATEGORY_SENSOR, CATEGORY_BRIDGE

STATE_DIR = Path(os.path.expanduser("~/.ruview-hap"))
STATE_DIR.mkdir(exist_ok=True)
STATE_FILE = STATE_DIR / "accessory.state"
SETUP_CODE_FILE = STATE_DIR / "setup-code.txt"
TOGGLE_FILE = Path(os.environ.get("RUVIEW_MOTION_TOGGLE", "/tmp/ruview-motion"))


class RuViewMotion(Accessory):
    category = CATEGORY_SENSOR

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        serv = self.add_preload_service("MotionSensor")
        self.char_motion = serv.configure_char("MotionDetected")
        self._last = False

    @Accessory.run_at_interval(1.0)
    def run(self):
        present = TOGGLE_FILE.exists()
        if present != self._last:
            self.char_motion.set_value(present)
            self._last = present
            print(
                f"[hap-test] MotionDetected -> {present}  (toggle file: {TOGGLE_FILE})",
                flush=True,
            )


def main() -> int:
    driver = AccessoryDriver(port=51826, persist_file=str(STATE_FILE))

    bridge = Bridge(driver, "RuView Test Bridge")
    bridge.category = CATEGORY_BRIDGE
    bridge.add_accessory(RuViewMotion(driver, "RuView Test Motion"))
    driver.add_accessory(accessory=bridge)

    setup_code = driver.state.pincode.decode() if hasattr(driver.state.pincode, "decode") else driver.state.pincode
    SETUP_CODE_FILE.write_text(str(setup_code) + "\n")
    print(f"[hap-test] HAP bridge advertising as 'RuView Test Bridge'")
    print(f"[hap-test] iPhone pair flow: Home app -> Add Accessory -> More Options")
    print(f"[hap-test] Setup code (also in {SETUP_CODE_FILE}):  {setup_code}")
    print(f"[hap-test] Motion toggle file: {TOGGLE_FILE}")
    print(f"[hap-test] State persists in: {STATE_FILE}")

    signal.signal(signal.SIGTERM, lambda *_: driver.stop())
    driver.start()
    return 0


if __name__ == "__main__":
    sys.exit(main())
