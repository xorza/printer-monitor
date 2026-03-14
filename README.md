# printer-monitor

Monitors a Prusa Core One 3D printer for print failures using ML-based detection. Captures webcam snapshots, runs them through [Obico](https://www.obico.io/) failure detection, and automatically pauses the print if a failure is detected.

## How it works

```
Poll PrusaLink → Capture RTSP snapshot → Obico ML detection → Temporal smoothing → Pause + Telegram alert
```

1. Every 10 seconds, checks printer status via PrusaLink API
2. Captures a frame from the RTSP camera stream using ffmpeg
3. Sends the frame to the Obico ML API for failure detection
4. Applies temporal smoothing (EWM + rolling baselines) to filter noise
5. On sustained failure detection: pauses the print via PrusaLink and sends a Telegram notification with a snapshot

Detection uses a two-tier escalation — **Warning** (notification only) and **Failing** (pause print) — with a 5-minute grace period at print start and configurable sensitivity.

## Telegram bot commands

- `/status` — capture and send a snapshot with current printer state
- `/pause` — pause the current print
- `/resume` — resume a paused print

## Setup

### Requirements

- Docker and Docker Compose
- A Prusa printer with PrusaLink enabled (optional — without it, detection still runs but can't pause)
- An RTSP camera pointed at the print bed
- A Telegram bot token ([create one](https://core.telegram.org/bots#botfather))

### Configuration

Copy `.env.example` to `.env` and fill in:

```bash
cp .env.example .env
```

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `PRUSALINK_URL` | No* | — | PrusaLink API URL (e.g. `http://192.168.0.10`) |
| `PRUSALINK_API_KEY` | No* | — | PrusaLink API key |
| `RTSP_URL` | Yes | — | RTSP camera stream URL |
| `OBICO_URL` | Yes | — | Obico ML API endpoint (use `http://obico-ml-api:3333` with included compose) |
| `OBICO_IMAGE_HOST` | Yes | — | Image server address as `host:port` (use `printer-monitor:8099` with included compose) |
| `TELEGRAM_BOT_TOKEN` | Yes | — | Telegram bot token |
| `TELEGRAM_CHAT_ID` | Yes | — | Telegram chat ID (numeric) |
| `DETECTION_SENSITIVITY` | No | `1.0` | Detection sensitivity multiplier (0.1–5.0) |

\* `PRUSALINK_URL` and `PRUSALINK_API_KEY` must both be set or both omitted. Without them, the service monitors and notifies but cannot pause/resume prints.

### Run

```bash
docker compose up -d
```

This starts two services:
- **obico-ml-api** — Obico's ML detection model
- **printer-monitor** — the monitoring service

### Use pre-built image

```yaml
# docker-compose.yml
services:
  printer-monitor:
    image: ghcr.io/xorza/printer-monitor:main
    env_file: .env
    # ...
```

## Build from source

```bash
cargo build --release
cargo nextest run
```

## License

MIT
