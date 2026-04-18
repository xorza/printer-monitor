# printer-monitor

Monitors a 3D printer for print failures using ML-based detection. Captures webcam snapshots, runs them through [Obico](https://www.obico.io/) failure detection, and automatically pauses the print if a failure is detected. Also drives a scheduled stealth-mode on/off so the printer is quiet at night.

## How it works

```
Poll PrusaLink → Capture RTSP snapshot → Obico ML detection → Temporal smoothing → Pause + Telegram alert
```

1. Every 10 seconds, checks printer status via PrusaLink API.
2. Captures a frame from the RTSP camera stream (pure Rust: retina + openh264, no ffmpeg dependency).
3. Sends the frame to the Obico ML API for failure detection.
4. Applies temporal smoothing — an EWM of recent confidence minus a long-term baseline — to filter noise.
5. On sustained failure detection: pauses the print via PrusaLink (if auto-pause is on) and sends a Telegram notification with a snapshot.

Detection uses a two-tier escalation — **Warning** (notification only) and **Failing** (pause print) — with a ~100-second grace period at print start and a configurable sensitivity multiplier.

The same loop also drives the optional stealth-mode schedule. Configure `off_at` and `on_at` times in `settings.toml`; the service flips stealth at each boundary and retries every 10 s if the printer is offline, until the next boundary.

## Telegram bot commands

- `/status` — snapshot + current printer state + detection score
- `/pause` — pause the current print
- `/resume` — resume a paused print
- `/stealth [on|off]` — query or set stealth mode directly
- `/monitor [on|off]` — toggle failure monitoring (persisted)
- `/autopause [on|off]` — toggle auto-pause on failure (persisted)
- `/stealthschedule [on|off]` — toggle the scheduled stealth transitions (persisted)

All toggle commands accept `on|off|1|0|true|false`. With no argument they show the current state and inline buttons.

## Quick start

### Requirements

- Docker and Docker Compose
- A printer with PrusaLink enabled (optional — without it, detection still runs but can't pause)
- An RTSP camera pointed at the print bed
- A Telegram bot token ([create one](https://core.telegram.org/bots#botfather))

### 1. Clone and configure

```bash
git clone https://github.com/xorza/printer-monitor.git
cd printer-monitor
cp .env.example .env
```

Edit `.env` with your values:

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `PRUSALINK_URL` | No\* | — | PrusaLink API URL (e.g. `http://192.168.0.10`) |
| `PRUSALINK_API_KEY` | No\* | — | PrusaLink API key |
| `RTSP_URL` | Yes | — | RTSP camera stream URL |
| `TELEGRAM_BOT_TOKEN` | Yes | — | Telegram bot token |
| `TELEGRAM_CHAT_ID` | Yes | — | Your numeric chat ID ([get it from @userinfobot](https://t.me/userinfobot)) |
| `DETECTION_SENSITIVITY` | No | `1.0` | Detection sensitivity multiplier (0.1–5.0) |
| `TZ` | No | `UTC` | Time zone for the stealth schedule (e.g. `Europe/Paris`). Leave unset = UTC. |

\* `PRUSALINK_URL` and `PRUSALINK_API_KEY` must both be set or both omitted. Without them, the service monitors and notifies but cannot pause/resume or control stealth.

The remaining variables (`OBICO_URL`, `OBICO_IMAGE_HOST`) have working defaults for the included Docker Compose setup — no need to change them.

### 2. Run

```bash
docker compose up -d
```

This starts two services:
- **obico-ml-api** — Obico's ML detection model
- **printer-monitor** — the monitoring service (pre-built image from `ghcr.io/xorza/printer-monitor:main`)

### Persisted settings

Runtime toggles live in `settings.toml` inside the mounted data volume (`./volume/data/settings.toml`). On first start the file is created with defaults; edit it to change schedule times:

```toml
monitoring_enabled = true
auto_pause = true

[stealth_schedule]
enabled = false
off_at = "08:00"   # stealth OFF at 8am local time
on_at = "20:00"    # stealth ON at 8pm local time
```

Times are `"HH:MM"` (1- or 2-digit hour, 2-digit minute). Toggling via Telegram also writes back here.

### Build from source

Requires a Rust toolchain supporting edition 2024 (Rust 1.85+).

```bash
cargo build --release
cargo nextest run
```

## License

[MIT](LICENSE)
