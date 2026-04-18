pub mod config;
pub mod detection;
pub mod obico;
pub mod prusalink;
pub mod rtsp_capture;
pub mod schedule;
pub mod server;
pub mod settings;
pub mod telegram;

use std::sync::Arc;
use std::time::Duration;

use detection::{DetectionResult, DetectionState};
use prusalink::{JobStatus, PrinterState, PrusaLink, StatusResponse};
use schedule::{ScheduleAction, ScheduleConfigStatus, StealthSchedule, Window};
use server::ImageServer;
use teloxide::dispatching::{DefaultKey, HandlerExt, ShutdownToken};
use teloxide::prelude::*;
use teloxide::types::{ChatAction, InlineKeyboardButton, InlineKeyboardMarkup};
use teloxide::utils::command::BotCommands;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

#[derive(Debug, thiserror::Error)]
enum MonitorError {
    #[error("PrusaLink: {0}")]
    PrusaLink(#[from] reqwest::Error),
    #[error("Capture: {0}")]
    Capture(#[from] rtsp_capture::CaptureError),
    #[error("Obico: {0}")]
    Obico(#[from] obico::ObicoError),
    #[error("Telegram: {0}")]
    Telegram(#[from] teloxide::RequestError),
}

const POLL_TARGET: Duration = Duration::from_secs(10);
const POLL_MIN_SLEEP: Duration = Duration::from_secs(1);

/// Result of parsing a toggle-command argument (e.g. `/monitor on`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToggleArg {
    /// Unrecognized input — surface a usage message.
    Invalid,
    /// No argument — query current state and show buttons.
    Query,
    /// Explicit set: `/monitor on` → `Set(true)`, `/monitor off` → `Set(false)`.
    Set(bool),
}

fn parse_toggle(arg: &str) -> ToggleArg {
    match arg.trim().to_lowercase().as_str() {
        "on" | "1" | "true" => ToggleArg::Set(true),
        "off" | "0" | "false" => ToggleArg::Set(false),
        "" => ToggleArg::Query,
        _ => ToggleArg::Invalid,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AlertLevel {
    Safe,
    Warning,
    Failing,
}

#[derive(Debug)]
struct MonitorState {
    detection: DetectionState,
    printer_state: PrinterState,
    alert_level: AlertLevel,
    monitoring_enabled: bool,
    auto_pause: bool,
    stealth_schedule: StealthSchedule,
    last_applied_window: Option<Window>,
}

impl MonitorState {
    fn save_settings(&self) {
        settings::Settings {
            monitoring_enabled: self.monitoring_enabled,
            auto_pause: self.auto_pause,
            stealth_schedule: self.stealth_schedule.clone(),
        }
        .save();
    }
}

#[derive(Debug, Clone)]
struct AppState {
    prusa: Option<Arc<PrusaLink>>,
    obico: Arc<obico::Obico>,
    tg: Arc<telegram::Telegram>,
    camera: Arc<rtsp_capture::RtspCapture>,
    image_server: Arc<ImageServer>,
    monitor: Arc<Mutex<MonitorState>>,
}

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase")]
enum Command {
    /// Pause the current print
    Pause,
    /// Resume the paused print
    Resume,
    /// Take a snapshot and send it
    Status,
    /// Toggle stealth mode: /stealth [on|off|1|0|true|false]
    Stealth(String),
    /// Toggle failure monitoring: /monitor [on|off|1|0|true|false]
    Monitor(String),
    /// Toggle auto-pause on failure: /autopause [on|off|1|0|true|false]
    Autopause(String),
    /// Toggle scheduled stealth mode: /stealthschedule [on|off|1|0|true|false]
    Stealthschedule(String),
}

type SharedShutdownToken = Arc<Mutex<Option<ShutdownToken>>>;

fn main() {
    // Any panic — in any task, any thread — aborts the whole process.
    // Without this, a panic inside a tokio::spawn dies silently and leaves
    // the rest of the app running in a half-broken state.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        std::process::abort();
    }));

    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run());
}

async fn run() {
    let config = config::Config::from_env();
    let token = CancellationToken::new();

    let image_server = ImageServer::start(config.obico_image_port, token.clone()).await;
    info!("Image server started on {}", image_server.addr());

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let prusa = config
        .prusalink
        .map(|pl| Arc::new(PrusaLink::new(http_client.clone(), pl.url, pl.api_key)));
    if prusa.is_none() {
        warn!("PrusaLink not configured — pause/resume and print status disabled");
    }

    let state = AppState {
        prusa,
        obico: Arc::new(obico::Obico::new(
            http_client.clone(),
            &config.obico_url,
            &config.obico_image_host,
        )),
        tg: Arc::new(telegram::Telegram::new(
            http_client,
            config.telegram_bot_token,
            config.telegram_chat_id,
        )),
        camera: Arc::new(rtsp_capture::RtspCapture::new(&config.rtsp_url)),
        image_server: Arc::new(image_server),
        monitor: Arc::new(Mutex::new({
            let s = settings::Settings::load();
            log_schedule_config(&s.stealth_schedule);
            MonitorState {
                detection: DetectionState::new(config.detection_sensitivity),
                printer_state: PrinterState::Idle,
                alert_level: AlertLevel::Safe,
                monitoring_enabled: s.monitoring_enabled,
                auto_pause: s.auto_pause,
                stealth_schedule: s.stealth_schedule,
                last_applied_window: None,
            }
        })),
    };

    spawn_signal_handler(&token);
    let shutdown_token = spawn_telegram_dispatcher(&state, &token);

    loop {
        let started = tokio::time::Instant::now();
        if let Err(e) = monitor_cycle(&state).await {
            error!("Monitor error: {e}");
        }
        let sleep = POLL_TARGET
            .saturating_sub(started.elapsed())
            .max(POLL_MIN_SLEEP);
        tokio::select! {
            _ = token.cancelled() => break,
            _ = tokio::time::sleep(sleep) => {}
        }
    }

    if let Some(token) = shutdown_token.lock().await.take() {
        match token.shutdown() {
            Ok(fut) => fut.await,
            Err(e) => error!("Bot shutdown error: {e:?}"),
        }
    }

    info!("Shutdown complete.");
}

fn spawn_signal_handler(token: &CancellationToken) {
    let token = token.clone();
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        tokio::select! {
            _ = ctrl_c => info!("Received SIGINT, shutting down..."),
            _ = sigterm.recv() => info!("Received SIGTERM, shutting down..."),
        }
        token.cancel();
    });
}

fn spawn_telegram_dispatcher(state: &AppState, cancel: &CancellationToken) -> SharedShutdownToken {
    let shutdown_token: SharedShutdownToken = Arc::new(Mutex::new(None));

    tokio::spawn({
        let cancel = cancel.clone();
        let state = state.clone();
        let shutdown_token = shutdown_token.clone();
        async move {
            loop {
                let mut dispatcher = build_dispatcher(&state);
                *shutdown_token.lock().await = Some(dispatcher.shutdown_token());
                // Panics are handled by the global panic hook (aborts the
                // process), so we only observe graceful stops here.
                dispatcher.dispatch().await;
                if cancel.is_cancelled() {
                    break;
                }
                error!("Telegram dispatcher stopped, restarting in 5s...");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    });

    shutdown_token
}

fn build_dispatcher(state: &AppState) -> Dispatcher<Bot, teloxide::RequestError, DefaultKey> {
    let allowed_chat = state.tg.chat_id();
    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter(move |msg: Message| msg.chat.id == allowed_chat)
                .filter_command::<Command>()
                .endpoint(handle_command),
        )
        .branch(
            Update::filter_callback_query()
                .filter(move |q: CallbackQuery| {
                    q.message
                        .as_ref()
                        .is_some_and(|m| m.chat().id == allowed_chat)
                })
                .endpoint(handle_callback),
        );
    Dispatcher::builder(state.tg.bot().clone(), handler)
        .dependencies(dptree::deps![state.clone()])
        .build()
}

async fn monitor_cycle(state: &AppState) -> Result<(), MonitorError> {
    tick_stealth_schedule(state).await;

    let status = poll_printer(state).await?;

    if !state.monitor.lock().await.monitoring_enabled {
        return Ok(());
    }

    let Some(status) = status else {
        return Ok(());
    };

    let job = status.job.as_ref();
    if let Some(job) = job {
        info!(job_id = job.id, "{}", format_job_info(job));
    }

    let jpeg = state.camera.capture().await?;
    state.image_server.set_image(jpeg.clone());

    let obico_result = state.obico.detect().await?;

    let Some((alert, score, auto_pause)) = process_detection(state, &obico_result, job).await
    else {
        return Ok(());
    };

    let paused = try_pause(state, alert, auto_pause, job).await;
    send_alert(state, jpeg, score, paused, job).await
}

fn log_schedule_config(s: &StealthSchedule) {
    match schedule::validate_schedule_times(s) {
        ScheduleConfigStatus::Disabled => info!("Stealth schedule: disabled"),
        ScheduleConfigStatus::Ok => info!(
            "Stealth schedule: enabled — stealth OFF at {}, ON at {} (local time)",
            s.off_at, s.on_at
        ),
        ScheduleConfigStatus::InvalidTimes => warn!(
            "Stealth schedule is enabled but times don't parse: off_at={:?}, on_at={:?}. \
             Schedule will not fire. Use HH:MM format (e.g. \"08:00\").",
            s.off_at, s.on_at
        ),
    }
}

/// Apply the scheduled stealth state for the current window. NoOp when the
/// schedule is disabled, times don't parse, or the window hasn't changed
/// since the last successful apply. On PrusaLink failure, `last_applied_window`
/// is left untouched so the next 10s tick retries — this is the "printer was
/// offline at 8am" recovery path.
async fn tick_stealth_schedule(state: &AppState) {
    let Some(prusa) = &state.prusa else {
        return;
    };
    let action = {
        let mon = state.monitor.lock().await;
        schedule::schedule_action(
            &mon.stealth_schedule,
            mon.last_applied_window,
            chrono::Local::now().time(),
        )
    };
    let ScheduleAction::Apply(window) = action else {
        return;
    };
    let stealth_on = window.stealth_on();
    match prusa.set_stealth(stealth_on).await {
        Ok(()) => {
            info!(?window, stealth_on, "Applied scheduled stealth");
            state.monitor.lock().await.last_applied_window = Some(window);
        }
        Err(e) => {
            warn!(?window, "Scheduled stealth apply failed, will retry: {e}");
        }
    }
}

/// Classifies the (prev, current) printer-state pair so the I/O wrapper
/// knows whether to keep going, notify the user, or just reset state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transition {
    /// Currently printing — detection continues, no action needed.
    Active,
    /// Was printing, now not. Reset detection AND notify the user with the
    /// terminal state (so they see "Print stopped — printer is now Paused").
    Stopped(PrinterState),
    /// Wasn't printing, still isn't. Reset detection; don't notify (no print
    /// was active to "stop").
    Idle,
}

fn transition(prev: PrinterState, current: PrinterState) -> Transition {
    match (prev, current) {
        (_, PrinterState::Printing) => Transition::Active,
        (PrinterState::Printing, stopped) => Transition::Stopped(stopped),
        _ => Transition::Idle,
    }
}

/// Poll printer and update state. Handles non-printing transitions
/// (reset detection, notify on stop). Returns `None` if not printing.
async fn poll_printer(state: &AppState) -> Result<Option<StatusResponse>, MonitorError> {
    let Some(prusa) = &state.prusa else {
        return Ok(None);
    };

    let status = prusa.status().await?;
    let current = status.printer.state;
    info!(state = ?current, "Printer status");

    let t = {
        let mut mon = state.monitor.lock().await;
        let t = transition(mon.printer_state, current);
        mon.printer_state = current;
        if !matches!(t, Transition::Active) {
            mon.detection.reset_short_term();
            mon.alert_level = AlertLevel::Safe;
        }
        t
    };

    match t {
        Transition::Active => Ok(Some(status)),
        Transition::Stopped(stopped_at) => {
            notify_print_stopped(state, stopped_at).await?;
            Ok(None)
        }
        Transition::Idle => Ok(None),
    }
}

async fn notify_print_stopped(
    state: &AppState,
    printer_state: PrinterState,
) -> Result<(), MonitorError> {
    let msg = format!("Print stopped — printer is now {printer_state:?}");
    info!("{msg}");
    match state.camera.capture().await {
        Ok(jpeg) => state.tg.send_photo(jpeg, &msg, &[]).await?,
        Err(e) => {
            error!("Failed to capture snapshot for status change: {e}");
            state.tg.send_message(&msg).await?;
        }
    }
    Ok(())
}

/// Decide what the caller should do with `MonitorState.alert_level` given
/// the current level and a new detection result. Pure — caller applies.
#[derive(Debug, Clone, Copy, PartialEq)]
enum EscalationAction {
    /// Alert level stays put.
    None,
    /// Detection came back clean — drop alert_level to Safe.
    Reset,
    /// New higher alert level — raise alert_level and notify.
    Escalate { level: AlertLevel, score: f64 },
}

fn check_escalation(current: AlertLevel, result: DetectionResult) -> EscalationAction {
    let (level, score) = match result {
        DetectionResult::Safe => return EscalationAction::Reset,
        DetectionResult::Warning { score } => (AlertLevel::Warning, score),
        DetectionResult::Failing { score } => (AlertLevel::Failing, score),
    };
    if level <= current {
        EscalationAction::None
    } else {
        EscalationAction::Escalate { level, score }
    }
}

async fn process_detection(
    state: &AppState,
    obico_result: &obico::DetectionResponse,
    job: Option<&JobStatus>,
) -> Option<(AlertLevel, f64, bool)> {
    let mut mon = state.monitor.lock().await;
    let result = mon
        .detection
        .update(&obico_result.detections, job.map(|j| j.id));
    let escalation = match check_escalation(mon.alert_level, result) {
        EscalationAction::None => None,
        EscalationAction::Reset => {
            mon.alert_level = AlertLevel::Safe;
            None
        }
        EscalationAction::Escalate { level, score } => {
            mon.alert_level = level;
            Some((level, score, mon.auto_pause))
        }
    };

    match &escalation {
        Some((level, score, _)) => error!(?level, score, "Detection alert escalated"),
        None => info!("Detection: no escalation"),
    }

    escalation
}

fn should_pause(alert: AlertLevel, auto_pause: bool) -> bool {
    alert == AlertLevel::Failing && auto_pause
}

async fn try_pause(
    state: &AppState,
    alert: AlertLevel,
    auto_pause: bool,
    job: Option<&JobStatus>,
) -> bool {
    if !should_pause(alert, auto_pause) {
        return false;
    }
    let Some((prusa, job)) = state.prusa.as_ref().zip(job) else {
        return false;
    };
    match prusa.pause(job.id).await {
        Ok(()) => true,
        Err(e) => {
            error!("Failed to pause print: {e}");
            false
        }
    }
}

fn build_alert_caption(score: f64, paused: bool, job: Option<&JobStatus>) -> String {
    let action = if paused {
        "Print has been paused."
    } else {
        "Print is still running — monitor closely."
    };
    let job_line = job
        .map(|j| format!("{}\n", format_job_info(j)))
        .unwrap_or_default();
    format!("Print failure detected!\n{job_line}Score: {score:.2}\n{action}")
}

fn alert_buttons(paused: bool, has_prusa: bool) -> Vec<InlineKeyboardButton> {
    match (paused, has_prusa) {
        (true, _) => vec![InlineKeyboardButton::callback("Resume", "resume")],
        (false, true) => vec![InlineKeyboardButton::callback("Pause", "pause")],
        (false, false) => vec![],
    }
}

async fn send_alert(
    state: &AppState,
    jpeg: Vec<u8>,
    score: f64,
    paused: bool,
    job: Option<&JobStatus>,
) -> Result<(), MonitorError> {
    let caption = build_alert_caption(score, paused, job);
    let buttons = alert_buttons(paused, state.prusa.is_some());
    state.tg.send_photo(jpeg, &caption, &buttons).await?;
    Ok(())
}

async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    state: AppState,
) -> Result<(), teloxide::RequestError> {
    let chat = msg.chat.id;
    match cmd {
        Command::Pause | Command::Resume => {
            bot.send_chat_action(chat, ChatAction::Typing).await?;
            let reply = match &state.prusa {
                Some(prusa) => handle_pause_resume(prusa, matches!(cmd, Command::Pause)).await,
                None => "PrusaLink not configured.".to_string(),
            };
            bot.send_message(chat, reply).await?;
        }
        Command::Status => {
            bot.send_chat_action(chat, ChatAction::UploadPhoto).await?;
            let (caption, snapshot) = tokio::join!(status_caption(&state), state.camera.capture());
            match snapshot {
                Ok(jpeg) => {
                    bot.send_photo(
                        chat,
                        teloxide::types::InputFile::memory(jpeg).file_name("snapshot.jpg"),
                    )
                    .caption(caption)
                    .await?;
                }
                Err(e) => {
                    error!("Camera error: {e}");
                    bot.send_message(chat, format!("Camera error: {e}\n\n{caption}"))
                        .await?;
                }
            }
        }
        Command::Stealth(ref arg)
        | Command::Monitor(ref arg)
        | Command::Autopause(ref arg)
        | Command::Stealthschedule(ref arg) => {
            let enable = match parse_toggle(arg) {
                ToggleArg::Set(v) => Some(v),
                ToggleArg::Query => None,
                ToggleArg::Invalid => {
                    let name = match &cmd {
                        Command::Stealth(_) => "stealth",
                        Command::Monitor(_) => "monitor",
                        Command::Autopause(_) => "autopause",
                        Command::Stealthschedule(_) => "stealthschedule",
                        _ => unreachable!(),
                    };
                    bot.send_message(chat, format!("Usage: /{name} [on|off|1|0|true|false]"))
                        .await?;
                    return Ok(());
                }
            };
            match cmd {
                Command::Stealth(_) => {
                    bot.send_chat_action(chat, ChatAction::Typing).await?;
                    handle_stealth(&state, enable, &bot, chat).await?;
                }
                Command::Monitor(_) => {
                    handle_toggle(
                        &state,
                        enable,
                        "Failure monitoring",
                        "monitor",
                        |m| &mut m.monitoring_enabled,
                        &bot,
                        chat,
                    )
                    .await?;
                }
                Command::Autopause(_) => {
                    handle_toggle(
                        &state,
                        enable,
                        "Auto-pause",
                        "autopause",
                        |m| &mut m.auto_pause,
                        &bot,
                        chat,
                    )
                    .await?;
                }
                Command::Stealthschedule(_) => {
                    handle_schedule_toggle(&state, enable, &bot, chat).await?;
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(())
}

async fn handle_callback(
    bot: Bot,
    q: CallbackQuery,
    state: AppState,
) -> Result<(), teloxide::RequestError> {
    let data = q.data.as_deref().unwrap_or("");

    let reply = match data {
        "pause" | "resume" => {
            let pause = data == "pause";
            match &state.prusa {
                Some(prusa) => handle_pause_resume(prusa, pause).await,
                None => "PrusaLink not configured.".to_string(),
            }
        }
        "stealth on" | "stealth off" => {
            let enable = data == "stealth on";
            match &state.prusa {
                Some(prusa) => set_stealth_message(prusa, enable).await,
                None => "PrusaLink not configured.".to_string(),
            }
        }
        "monitor on" | "monitor off" => {
            let enable = data == "monitor on";
            set_toggle(&state, enable, "Failure monitoring", |m| {
                &mut m.monitoring_enabled
            })
            .await
        }
        "autopause on" | "autopause off" => {
            let enable = data == "autopause on";
            set_toggle(&state, enable, "Auto-pause", |m| &mut m.auto_pause).await
        }
        "stealthschedule on" | "stealthschedule off" => {
            let enable = data == "stealthschedule on";
            set_schedule_toggle(&state, enable).await
        }
        _ => {
            bot.answer_callback_query(q.id).await?;
            return Ok(());
        }
    };

    if let Some(msg) = &q.message {
        bot.edit_message_reply_markup(msg.chat().id, msg.id())
            .await
            .ok();
    }

    bot.answer_callback_query(q.id).text(&reply).await?;
    Ok(())
}

async fn handle_pause_resume(prusa: &PrusaLink, pause: bool) -> String {
    let action = if pause { "pause" } else { "resume" };
    let status = match prusa.status().await {
        Ok(s) => s,
        Err(e) => {
            error!("PrusaLink status error: {e}");
            return format!("Failed to get status: {e}");
        }
    };
    let job = match &status.job {
        Some(j) => j,
        None => return format!("No active job to {action}."),
    };
    let result = if pause {
        prusa.pause(job.id).await
    } else {
        prusa.resume(job.id).await
    };
    match result {
        Ok(()) => format!("Print {action}d."),
        Err(e) => {
            error!("PrusaLink {action} error: {e}");
            format!("Failed to {action}: {e}")
        }
    }
}

async fn set_stealth_message(prusa: &PrusaLink, enable: bool) -> String {
    match prusa.set_stealth(enable).await {
        Ok(()) => {
            let label = if enable { "enabled" } else { "disabled" };
            format!("Stealth mode {label}.")
        }
        Err(e) => {
            error!("Stealth set error: {e}");
            format!("Failed to set stealth: {e}")
        }
    }
}

async fn handle_stealth(
    state: &AppState,
    enable: Option<bool>,
    bot: &Bot,
    chat: ChatId,
) -> Result<(), teloxide::RequestError> {
    let Some(prusa) = &state.prusa else {
        bot.send_message(chat, "PrusaLink not configured.").await?;
        return Ok(());
    };

    if let Some(enable) = enable {
        let msg = set_stealth_message(prusa, enable).await;
        bot.send_message(chat, msg).await?;
    } else {
        match prusa.stealth().await {
            Ok(resp) => {
                let status = if resp.enabled { "ON" } else { "OFF" };
                let buttons = InlineKeyboardMarkup::new(vec![vec![
                    InlineKeyboardButton::callback("On", "stealth on"),
                    InlineKeyboardButton::callback("Off", "stealth off"),
                ]]);
                bot.send_message(chat, format!("Stealth mode is {status}."))
                    .reply_markup(buttons)
                    .await?;
            }
            Err(e) => {
                error!("Stealth get error: {e}");
                bot.send_message(chat, format!("Failed to get stealth state: {e}"))
                    .await?;
            }
        }
    }
    Ok(())
}

async fn set_toggle(
    state: &AppState,
    enable: bool,
    label: &str,
    field: fn(&mut MonitorState) -> &mut bool,
) -> String {
    let mut mon = state.monitor.lock().await;
    *field(&mut mon) = enable;
    if mon.monitoring_enabled {
        mon.alert_level = AlertLevel::Safe;
    }
    mon.save_settings();
    let action = if enable { "enabled" } else { "disabled" };
    format!("{label} {action}.")
}

async fn handle_toggle(
    state: &AppState,
    enable: Option<bool>,
    label: &str,
    callback_prefix: &str,
    field: fn(&mut MonitorState) -> &mut bool,
    bot: &Bot,
    chat: ChatId,
) -> Result<(), teloxide::RequestError> {
    if let Some(enable) = enable {
        let msg = set_toggle(state, enable, label, field).await;
        bot.send_message(chat, msg).await?;
    } else {
        let enabled = *field(&mut *state.monitor.lock().await);
        let status = if enabled { "ON" } else { "OFF" };
        let buttons = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("On", format!("{callback_prefix} on")),
            InlineKeyboardButton::callback("Off", format!("{callback_prefix} off")),
        ]]);
        bot.send_message(chat, format!("{label} is {status}."))
            .reply_markup(buttons)
            .await?;
    }
    Ok(())
}

/// Toggle the stealth schedule enabled flag. Always clears
/// `last_applied_window` so the next monitor tick re-applies (otherwise a
/// disable/enable cycle could leave the printer out of sync with the schedule
/// until the next boundary).
async fn set_schedule_toggle(state: &AppState, enable: bool) -> String {
    let mut mon = state.monitor.lock().await;
    mon.stealth_schedule.enabled = enable;
    mon.last_applied_window = None;
    let status = schedule::validate_schedule_times(&mon.stealth_schedule);
    mon.save_settings();
    let action = if enable { "enabled" } else { "disabled" };
    let mut msg = format!("Stealth schedule {action}.");
    if status == ScheduleConfigStatus::InvalidTimes {
        msg.push_str(
            "\nWarning: configured times don't parse — schedule will not fire. \
             Fix off_at/on_at in settings.toml (HH:MM format).",
        );
    }
    msg
}

async fn handle_schedule_toggle(
    state: &AppState,
    enable: Option<bool>,
    bot: &Bot,
    chat: ChatId,
) -> Result<(), teloxide::RequestError> {
    if let Some(enable) = enable {
        let msg = set_schedule_toggle(state, enable).await;
        bot.send_message(chat, msg).await?;
    } else {
        let mon = state.monitor.lock().await;
        let status = if mon.stealth_schedule.enabled {
            "ON"
        } else {
            "OFF"
        };
        let msg = format!(
            "Stealth schedule is {status}.\nStealth OFF at {}, ON at {}.",
            mon.stealth_schedule.off_at, mon.stealth_schedule.on_at
        );
        drop(mon);
        let buttons = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("On", "stealthschedule on"),
            InlineKeyboardButton::callback("Off", "stealthschedule off"),
        ]]);
        bot.send_message(chat, msg).reply_markup(buttons).await?;
    }
    Ok(())
}

fn format_status_caption(
    printer_state: PrinterState,
    job: Option<&JobStatus>,
    score: f64,
) -> String {
    let job_info = job
        .map(format_job_info)
        .unwrap_or_else(|| "No active job".to_string());
    format!("State: {printer_state:?}\n{job_info}\nDetection score: {score:.2}")
}

async fn status_caption(state: &AppState) -> String {
    let score = state.monitor.lock().await.detection.current_score();

    let Some(prusa) = state.prusa.as_ref() else {
        return format!("PrusaLink not configured.\nDetection score: {score:.2}");
    };
    match prusa.status().await {
        Ok(status) => format_status_caption(status.printer.state, status.job.as_ref(), score),
        Err(e) => {
            error!("PrusaLink status error: {e}");
            format!("PrusaLink error: {e}\nDetection score: {score:.2}")
        }
    }
}

fn format_job_info(job: &JobStatus) -> String {
    format!(
        "Job #{}, progress: {:.1}%",
        job.id,
        job.progress.unwrap_or(0.0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u64, progress: Option<f64>) -> JobStatus {
        JobStatus {
            id,
            progress,
            time_remaining: None,
            time_printing: None,
        }
    }

    #[test]
    fn parse_toggle_on_variants() {
        assert_eq!(parse_toggle("on"), ToggleArg::Set(true));
        assert_eq!(parse_toggle("1"), ToggleArg::Set(true));
        assert_eq!(parse_toggle("true"), ToggleArg::Set(true));
        assert_eq!(parse_toggle("ON"), ToggleArg::Set(true));
        assert_eq!(parse_toggle("True"), ToggleArg::Set(true));
        assert_eq!(parse_toggle("  on  "), ToggleArg::Set(true));
    }

    #[test]
    fn parse_toggle_off_variants() {
        assert_eq!(parse_toggle("off"), ToggleArg::Set(false));
        assert_eq!(parse_toggle("0"), ToggleArg::Set(false));
        assert_eq!(parse_toggle("false"), ToggleArg::Set(false));
        assert_eq!(parse_toggle("OFF"), ToggleArg::Set(false));
    }

    #[test]
    fn parse_toggle_empty_is_query() {
        assert_eq!(parse_toggle(""), ToggleArg::Query);
        assert_eq!(parse_toggle("  "), ToggleArg::Query);
    }

    #[test]
    fn parse_toggle_invalid() {
        assert_eq!(parse_toggle("yes"), ToggleArg::Invalid);
        assert_eq!(parse_toggle("no"), ToggleArg::Invalid);
        assert_eq!(parse_toggle("2"), ToggleArg::Invalid);
        assert_eq!(parse_toggle("maybe"), ToggleArg::Invalid);
    }

    #[test]
    fn format_job_info_with_progress() {
        let j = job(42, Some(73.456));
        assert_eq!(format_job_info(&j), "Job #42, progress: 73.5%");
    }

    #[test]
    fn format_job_info_no_progress() {
        let j = job(1, None);
        assert_eq!(format_job_info(&j), "Job #1, progress: 0.0%");
    }

    #[test]
    fn alert_caption_paused_with_job() {
        let j = job(5, Some(50.0));
        let caption = build_alert_caption(0.72, true, Some(&j));
        assert!(caption.contains("Print failure detected!"));
        assert!(caption.contains("Job #5, progress: 50.0%"));
        assert!(caption.contains("Score: 0.72"));
        assert!(caption.contains("Print has been paused."));
    }

    #[test]
    fn alert_caption_not_paused_no_job() {
        let caption = build_alert_caption(0.40, false, None);
        assert!(caption.contains("Score: 0.40"));
        assert!(caption.contains("Print is still running"));
        assert!(!caption.contains("Job #"));
    }

    #[test]
    fn alert_buttons_paused() {
        let buttons = alert_buttons(true, true);
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0].text, "Resume");
    }

    #[test]
    fn alert_buttons_not_paused_with_prusa() {
        let buttons = alert_buttons(false, true);
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0].text, "Pause");
    }

    #[test]
    fn alert_buttons_not_paused_no_prusa() {
        let buttons = alert_buttons(false, false);
        assert!(buttons.is_empty());
    }

    #[test]
    fn status_caption_formatting() {
        let j = job(10, Some(25.0));
        let caption = format_status_caption(PrinterState::Printing, Some(&j), 0.15);
        assert!(caption.contains("State: Printing"));
        assert!(caption.contains("Job #10, progress: 25.0%"));
        assert!(caption.contains("Detection score: 0.15"));
    }

    #[test]
    fn status_caption_no_job() {
        let caption = format_status_caption(PrinterState::Idle, None, 0.0);
        assert!(caption.contains("State: Idle"));
        assert!(caption.contains("No active job"));
        assert!(caption.contains("Detection score: 0.00"));
    }

    #[test]
    fn alert_level_ordering() {
        assert!(AlertLevel::Safe < AlertLevel::Warning);
        assert!(AlertLevel::Warning < AlertLevel::Failing);
        assert!(AlertLevel::Safe < AlertLevel::Failing);
    }

    #[test]
    fn alert_level_no_escalation_when_equal() {
        assert!(AlertLevel::Warning <= AlertLevel::Warning);
        assert!(AlertLevel::Failing <= AlertLevel::Failing);
    }

    #[test]
    fn escalation_safe_is_reset() {
        assert_eq!(
            check_escalation(AlertLevel::Warning, DetectionResult::Safe),
            EscalationAction::Reset,
        );
        // Reset is returned even if already Safe — caller writes Safe either way.
        assert_eq!(
            check_escalation(AlertLevel::Safe, DetectionResult::Safe),
            EscalationAction::Reset,
        );
    }

    #[test]
    fn escalation_safe_to_warning() {
        assert_eq!(
            check_escalation(AlertLevel::Safe, DetectionResult::Warning { score: 0.4 }),
            EscalationAction::Escalate {
                level: AlertLevel::Warning,
                score: 0.4,
            },
        );
    }

    #[test]
    fn escalation_warning_to_failing() {
        assert_eq!(
            check_escalation(AlertLevel::Warning, DetectionResult::Failing { score: 0.7 }),
            EscalationAction::Escalate {
                level: AlertLevel::Failing,
                score: 0.7,
            },
        );
    }

    #[test]
    fn escalation_suppressed_at_same_level() {
        assert_eq!(
            check_escalation(AlertLevel::Warning, DetectionResult::Warning { score: 0.5 }),
            EscalationAction::None,
        );
        assert_eq!(
            check_escalation(AlertLevel::Failing, DetectionResult::Failing { score: 0.9 }),
            EscalationAction::None,
        );
    }

    #[test]
    fn escalation_suppressed_at_higher_level() {
        // Failing state receives Warning — don't downgrade.
        assert_eq!(
            check_escalation(AlertLevel::Failing, DetectionResult::Warning { score: 0.4 }),
            EscalationAction::None,
        );
    }

    #[test]
    fn should_pause_only_on_failing_with_auto_pause() {
        assert!(should_pause(AlertLevel::Failing, true));
        assert!(!should_pause(AlertLevel::Failing, false));
        assert!(!should_pause(AlertLevel::Warning, true));
        assert!(!should_pause(AlertLevel::Warning, false));
        assert!(!should_pause(AlertLevel::Safe, true));
    }

    /// Simulates how `process_detection` applies `EscalationAction` to a
    /// running alert level across a sequence of detection results.
    #[test]
    fn escalation_sequence_safe_to_warning_to_failing_and_back() {
        fn apply(current: &mut AlertLevel, result: DetectionResult) -> EscalationAction {
            let action = check_escalation(*current, result);
            match action {
                EscalationAction::Reset => *current = AlertLevel::Safe,
                EscalationAction::Escalate { level, .. } => *current = level,
                EscalationAction::None => {}
            }
            action
        }

        let mut current = AlertLevel::Safe;

        assert_eq!(
            apply(&mut current, DetectionResult::Safe),
            EscalationAction::Reset
        );
        assert_eq!(current, AlertLevel::Safe);

        assert!(matches!(
            apply(&mut current, DetectionResult::Warning { score: 0.4 }),
            EscalationAction::Escalate {
                level: AlertLevel::Warning,
                ..
            }
        ));
        assert_eq!(current, AlertLevel::Warning);

        // Repeated warning suppressed
        assert_eq!(
            apply(&mut current, DetectionResult::Warning { score: 0.45 }),
            EscalationAction::None,
        );
        assert_eq!(current, AlertLevel::Warning);

        // Escalate Warning → Failing
        assert!(matches!(
            apply(&mut current, DetectionResult::Failing { score: 0.7 }),
            EscalationAction::Escalate {
                level: AlertLevel::Failing,
                ..
            }
        ));
        assert_eq!(current, AlertLevel::Failing);

        // Repeated failing suppressed
        assert_eq!(
            apply(&mut current, DetectionResult::Failing { score: 0.8 }),
            EscalationAction::None,
        );

        // Safe resets
        assert_eq!(
            apply(&mut current, DetectionResult::Safe),
            EscalationAction::Reset
        );
        assert_eq!(current, AlertLevel::Safe);

        // Can escalate again after reset
        assert!(matches!(
            apply(&mut current, DetectionResult::Warning { score: 0.35 }),
            EscalationAction::Escalate { .. }
        ));
    }

    // --- transition ---

    #[test]
    fn transition_idle_to_printing_is_active() {
        assert_eq!(
            transition(PrinterState::Idle, PrinterState::Printing),
            Transition::Active,
        );
    }

    #[test]
    fn transition_printing_to_printing_is_active() {
        assert_eq!(
            transition(PrinterState::Printing, PrinterState::Printing),
            Transition::Active,
        );
    }

    #[test]
    fn transition_printing_to_idle_stops() {
        assert_eq!(
            transition(PrinterState::Printing, PrinterState::Idle),
            Transition::Stopped(PrinterState::Idle),
        );
    }

    #[test]
    fn transition_printing_to_paused_stops() {
        assert_eq!(
            transition(PrinterState::Printing, PrinterState::Paused),
            Transition::Stopped(PrinterState::Paused),
        );
    }

    #[test]
    fn transition_printing_to_error_stops() {
        assert_eq!(
            transition(PrinterState::Printing, PrinterState::Error),
            Transition::Stopped(PrinterState::Error),
        );
    }

    #[test]
    fn transition_idle_to_idle() {
        assert_eq!(
            transition(PrinterState::Idle, PrinterState::Idle),
            Transition::Idle,
        );
    }

    #[test]
    fn transition_paused_to_error_is_idle() {
        // Neither end is Printing — no notification, just state reset.
        assert_eq!(
            transition(PrinterState::Paused, PrinterState::Error),
            Transition::Idle,
        );
    }
}
