use chrono::NaiveTime;
use serde::{Deserialize, Serialize};

// -----------------------------------------------------------------------------
// Input data
// -----------------------------------------------------------------------------

/// Persisted schedule config. Times are `"HH:MM"` strings (interpreted as
/// local time, driven by the container's `TZ` env var).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StealthSchedule {
    pub enabled: bool,
    pub off_at: String,
    pub on_at: String,
}

impl Default for StealthSchedule {
    fn default() -> Self {
        Self {
            enabled: false,
            off_at: "08:00".to_string(),
            on_at: "20:00".to_string(),
        }
    }
}

// -----------------------------------------------------------------------------
// Time parsing
// -----------------------------------------------------------------------------

/// Parse `"H:MM"` or `"HH:MM"`. Minute must be 2 digits (so `"8:5"` is rejected
/// — would be ambiguous with `"8:50"`). Hour is 0–23, minute is 0–59.
fn parse_hhmm(s: &str) -> Option<NaiveTime> {
    let s = s.trim();
    let (h, m) = s.split_once(':')?;
    if !(1..=2).contains(&h.len()) || m.len() != 2 {
        return None;
    }
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    NaiveTime::from_hms_opt(h, m, 0)
}

// -----------------------------------------------------------------------------
// Window (which half of the schedule `now` falls into)
// -----------------------------------------------------------------------------

/// The phase of the schedule at a given instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// Between `off_at` (inclusive) and `on_at` (exclusive). Stealth should be OFF.
    Day,
    /// Between `on_at` (inclusive) and `off_at` (exclusive). Stealth should be ON.
    Night,
}

impl Window {
    /// Stealth state implied by this window.
    pub fn stealth_on(self) -> bool {
        matches!(self, Window::Night)
    }
}

/// Window containing `now`.
///
/// Day   = [off_at, on_at)
/// Night = [on_at, off_at)
/// When off_at == on_at, treat the full day as Day (degenerate / no-op schedule).
fn current_window(now: NaiveTime, off_at: NaiveTime, on_at: NaiveTime) -> Window {
    if off_at == on_at {
        return Window::Day;
    }
    if off_at < on_at {
        if now >= off_at && now < on_at {
            Window::Day
        } else {
            Window::Night
        }
    } else if now >= on_at && now < off_at {
        Window::Night
    } else {
        Window::Day
    }
}

// -----------------------------------------------------------------------------
// Tick decision
// -----------------------------------------------------------------------------

/// What the tick should do this cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleAction {
    /// Schedule disabled, unparseable, or current window already applied.
    NoOp,
    /// Push stealth to `window.stealth_on()` and record `window` on success.
    Apply(Window),
}

/// Decide whether to push a new stealth state on this tick. `NoOp` if the
/// schedule is disabled, times don't parse, or the current window has already
/// been applied.
pub fn schedule_action(
    schedule: &StealthSchedule,
    last_applied: Option<Window>,
    now: NaiveTime,
) -> ScheduleAction {
    if !schedule.enabled {
        return ScheduleAction::NoOp;
    }
    let (Some(off), Some(on)) = (parse_hhmm(&schedule.off_at), parse_hhmm(&schedule.on_at)) else {
        return ScheduleAction::NoOp;
    };
    let window = current_window(now, off, on);
    if Some(window) == last_applied {
        return ScheduleAction::NoOp;
    }
    ScheduleAction::Apply(window)
}

// -----------------------------------------------------------------------------
// Config validation (startup / toggle diagnostics)
// -----------------------------------------------------------------------------

/// Surfaces whether the schedule's configured times are usable. Lets the
/// startup path and the `/stealthschedule` reply distinguish silent-noop
/// (disabled) from broken-config (enabled but unparseable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleConfigStatus {
    Disabled,
    Ok,
    InvalidTimes,
}

/// Check whether the configured times parse. Pure — no logging, no side effects.
pub fn validate_schedule_times(schedule: &StealthSchedule) -> ScheduleConfigStatus {
    if !schedule.enabled {
        return ScheduleConfigStatus::Disabled;
    }
    if parse_hhmm(&schedule.off_at).is_some() && parse_hhmm(&schedule.on_at).is_some() {
        ScheduleConfigStatus::Ok
    } else {
        ScheduleConfigStatus::InvalidTimes
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn sched(enabled: bool, off: &str, on: &str) -> StealthSchedule {
        StealthSchedule {
            enabled,
            off_at: off.to_string(),
            on_at: on.to_string(),
        }
    }

    // --- parse_hhmm ---

    #[test]
    fn parse_valid() {
        assert_eq!(parse_hhmm("08:00"), Some(t(8, 0)));
        assert_eq!(parse_hhmm("8:00"), Some(t(8, 0)));
        assert_eq!(parse_hhmm("0:00"), Some(t(0, 0)));
        assert_eq!(parse_hhmm("00:00"), Some(t(0, 0)));
        assert_eq!(parse_hhmm("23:59"), Some(t(23, 59)));
        assert_eq!(parse_hhmm("  20:30  "), Some(t(20, 30)));
    }

    #[test]
    fn parse_invalid() {
        assert_eq!(parse_hhmm(""), None);
        assert_eq!(parse_hhmm("08:0"), None); // single-digit minute is ambiguous (08:5 vs 08:50)
        assert_eq!(parse_hhmm("8:5"), None);
        assert_eq!(parse_hhmm("123:00"), None); // 3-digit hour
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("12:60"), None);
        assert_eq!(parse_hhmm("08-00"), None);
        assert_eq!(parse_hhmm("abcde"), None);
        assert_eq!(parse_hhmm("08:00:00"), None);
        assert_eq!(parse_hhmm(":00"), None);
        assert_eq!(parse_hhmm("08:"), None);
    }

    // --- Window::stealth_on ---

    #[test]
    fn window_stealth_mapping() {
        assert!(Window::Night.stealth_on());
        assert!(!Window::Day.stealth_on());
    }

    // --- current_window: typical case (off=08:00, on=20:00) ---

    #[test]
    fn window_typical_before_off() {
        assert_eq!(current_window(t(7, 59), t(8, 0), t(20, 0)), Window::Night);
    }

    #[test]
    fn window_typical_at_off_boundary() {
        assert_eq!(current_window(t(8, 0), t(8, 0), t(20, 0)), Window::Day);
    }

    #[test]
    fn window_typical_midday() {
        assert_eq!(current_window(t(14, 30), t(8, 0), t(20, 0)), Window::Day);
    }

    #[test]
    fn window_typical_just_before_on() {
        assert_eq!(current_window(t(19, 59), t(8, 0), t(20, 0)), Window::Day);
    }

    #[test]
    fn window_typical_at_on_boundary() {
        assert_eq!(current_window(t(20, 0), t(8, 0), t(20, 0)), Window::Night);
    }

    #[test]
    fn window_typical_late_night() {
        assert_eq!(current_window(t(23, 30), t(8, 0), t(20, 0)), Window::Night);
        assert_eq!(current_window(t(2, 0), t(8, 0), t(20, 0)), Window::Night);
    }

    // --- current_window: inverted case (off=22:00, on=06:00) — Day crosses midnight ---

    #[test]
    fn window_inverted_in_day_before_midnight() {
        assert_eq!(current_window(t(23, 0), t(22, 0), t(6, 0)), Window::Day);
    }

    #[test]
    fn window_inverted_in_day_after_midnight() {
        assert_eq!(current_window(t(3, 0), t(22, 0), t(6, 0)), Window::Day);
    }

    #[test]
    fn window_inverted_at_on_boundary() {
        assert_eq!(current_window(t(6, 0), t(22, 0), t(6, 0)), Window::Night);
    }

    #[test]
    fn window_inverted_midday_night() {
        assert_eq!(current_window(t(12, 0), t(22, 0), t(6, 0)), Window::Night);
    }

    #[test]
    fn window_inverted_at_off_boundary() {
        assert_eq!(current_window(t(22, 0), t(22, 0), t(6, 0)), Window::Day);
    }

    // --- current_window: degenerate case (off == on) ---

    #[test]
    fn window_degenerate_always_day() {
        assert_eq!(current_window(t(0, 0), t(8, 0), t(8, 0)), Window::Day);
        assert_eq!(current_window(t(8, 0), t(8, 0), t(8, 0)), Window::Day);
        assert_eq!(current_window(t(15, 0), t(8, 0), t(8, 0)), Window::Day);
    }

    // --- schedule_action ---

    #[test]
    fn action_disabled_is_noop() {
        let s = sched(false, "08:00", "20:00");
        assert_eq!(schedule_action(&s, None, t(10, 0)), ScheduleAction::NoOp);
    }

    #[test]
    fn action_parse_failure_is_noop() {
        let s = sched(true, "garbage", "20:00");
        assert_eq!(schedule_action(&s, None, t(10, 0)), ScheduleAction::NoOp);
    }

    #[test]
    fn action_fresh_state_applies_current_window() {
        let s = sched(true, "08:00", "20:00");
        assert_eq!(
            schedule_action(&s, None, t(10, 0)),
            ScheduleAction::Apply(Window::Day)
        );
        assert_eq!(
            schedule_action(&s, None, t(22, 0)),
            ScheduleAction::Apply(Window::Night)
        );
    }

    #[test]
    fn action_same_window_is_noop() {
        let s = sched(true, "08:00", "20:00");
        assert_eq!(
            schedule_action(&s, Some(Window::Day), t(10, 0)),
            ScheduleAction::NoOp
        );
        assert_eq!(
            schedule_action(&s, Some(Window::Night), t(22, 0)),
            ScheduleAction::NoOp
        );
    }

    #[test]
    fn action_window_change_triggers_apply() {
        let s = sched(true, "08:00", "20:00");
        assert_eq!(
            schedule_action(&s, Some(Window::Night), t(8, 0)),
            ScheduleAction::Apply(Window::Day)
        );
        assert_eq!(
            schedule_action(&s, Some(Window::Day), t(20, 0)),
            ScheduleAction::Apply(Window::Night)
        );
    }

    // --- validate_schedule_times ---

    #[test]
    fn validate_disabled() {
        let s = sched(false, "garbage", "also bad");
        assert_eq!(validate_schedule_times(&s), ScheduleConfigStatus::Disabled);
    }

    #[test]
    fn validate_ok() {
        let s = sched(true, "08:00", "20:00");
        assert_eq!(validate_schedule_times(&s), ScheduleConfigStatus::Ok);
    }

    #[test]
    fn validate_invalid_off() {
        let s = sched(true, "nope", "20:00");
        assert_eq!(
            validate_schedule_times(&s),
            ScheduleConfigStatus::InvalidTimes
        );
    }

    #[test]
    fn validate_invalid_on() {
        let s = sched(true, "08:00", "nope");
        assert_eq!(
            validate_schedule_times(&s),
            ScheduleConfigStatus::InvalidTimes
        );
    }

    // --- default ---

    #[test]
    fn default_schedule_disabled_with_sensible_times() {
        let s = StealthSchedule::default();
        assert!(!s.enabled);
        assert_eq!(parse_hhmm(&s.off_at), Some(t(8, 0)));
        assert_eq!(parse_hhmm(&s.on_at), Some(t(20, 0)));
    }
}
