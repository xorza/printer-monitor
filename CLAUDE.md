# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

Rust service monitoring a Prusa Core One 3D printer for print failures. Pipeline: poll PrusaLink status → capture RTSP webcam snapshot → run through Obico ML detection → if failure detected, pause print via PrusaLink and notify via Telegram bot. The same 10-second loop also drives a scheduled stealth-mode on/off (times configured in `settings.toml`) with automatic retry when the printer is offline.

Config (`.env`): PrusaLink URL/API key, Telegram bot token + chat id, Obico endpoint, RTSP URL, optional `TZ` for the stealth schedule. Runtime toggles (monitoring, auto-pause, stealth schedule) persist to `settings.toml` in the data volume. Deployed via Docker Compose.

## Commands

```bash
cargo nextest run && cargo fmt && cargo check && cargo clippy --all-targets -- -D warnings  # full verify (run after changes)
cargo build --release          # release build
cargo run                      # run locally
```

## Coding Rules

- `Result<>` only for expected failures (network, I/O, external services). No `Option`/`Result` wrapping for things that cannot fail.
- `.unwrap()` for required values; `.expect("reason")` for non-obvious cases. Crash on logic errors, never swallow them.
- Assert function inputs/outputs to catch logic errors (not user input or network failures).
- `#[derive(Debug)]` on all structs.
- No backward compatibility — remove old code, rename freely, rewrite callers. No shims or wrappers.
- No `#[cfg(test)]` on production functions. Test helpers go in test modules.
- Remove unused code. If intentionally kept, comment why and silence linter.

## Testing

- Tests for ALL new/modified code. Must verify **correctness** with exact expected values (show math in comments), not vague ranges like `result < 10`.
- Cover edge cases: empty input, minimal input, boundaries.
- For parameterized code, test that different parameters produce different results.
- Skip doc-tests.
- **Prefer pure decision functions over mocking.** Extract the decision (see `schedule_action`, `check_escalation`, `transition`, `validate_schedule_times`) and let the I/O wrapper call it. Pure fns are unit-tested directly; the thin async wrappers that call them + await HTTP are left untested — they're trivial glue.

## Documentation

- Use `NOTES-AI.md` for AI-generated implementation notes (current state only, not history). Split if >300 lines.
- Don't edit `README.md` unless asked.
