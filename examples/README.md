# Examples

Real-time sensing applications built on the RuView platform.

| Example | Sensors | What It Does |
|---------|---------|-------------|
| [Medical: Blood Pressure](medical/) | mmWave (COM4) | Contactless BP estimation from HRV |
| [Sleep: Apnea Screener](sleep/) | mmWave (COM4) | Detects breathing cessation events, computes AHI |
| [Stress: HRV Monitor](stress/) | mmWave (COM4) | Real-time stress level from heart rate variability |
| [Environment: Room Monitor](environment/) | CSI (COM7) + mmWave (COM4) | Occupancy, light, RF fingerprint, activity events |

## Hardware Required

| Port | Device | Cost |
|------|--------|------|
| COM7 | ESP32-S3 (WiFi CSI) | ~$9 |
| COM4 | ESP32-C6 + Seeed MR60BHA2 (60 GHz mmWave) | ~$15 |

## Quick Start

```bash
pip install pyserial numpy

# Blood pressure
python examples/medical/bp_estimator.py --port COM4

# Sleep apnea screening
python examples/sleep/apnea_screener.py --port COM4 --duration 3600

# Stress monitoring
python examples/stress/hrv_stress_monitor.py --port COM4

# Full room monitor (both sensors)
python examples/environment/room_monitor.py --csi-port COM7 --mmwave-port COM4
```
