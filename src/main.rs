pub mod config;
pub mod detection;
pub mod obico;
pub mod prusalink;
pub mod rtsp_capture;
pub mod server;
pub mod settings;
pub mod telegram;

use std::sync::Arc;
use std::time::Duration;

use detection::{DetectionResult, DetectionState};
use prusalink::{JobStatus, PrinterState, PrusaLink, StatusResponse};
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

fn parse_toggle(arg: &str) -> Option<Option<bool>> {
    match arg.trim().to_lowercase().as_str() {
        "on" | "1" | "true" => Some(Some(true)),
        "off" | "0" | "false" => Some(Some(false)),
        "" => Some(None),
        _ => None,
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
}

impl MonitorState {
    fn save_settings(&self) {
        settings::Settings {
            monitoring_enabled: self.monitoring_enabled,
            auto_pause: self.auto_pause,
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
}

type SharedShutdownToken = Arc<Mutex<Option<ShutdownToken>>>;

fn main() {
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

    let image_server = ImageServer::start(&config.obico_image_host, token.clone()).await;
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
            MonitorState {
                detection: DetectionState::new(config.detection_sensitivity),
                printer_state: PrinterState::Idle,
                alert_level: AlertLevel::Safe,
                monitoring_enabled: s.monitoring_enabled,
                auto_pause: s.auto_pause,
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
                let result = tokio::task::spawn(async move { dispatcher.dispatch().await }).await;
                if cancel.is_cancelled() {
                    break;
                }
                match result {
                    Ok(()) => error!("Telegram dispatcher stopped, restarting in 5s..."),
                    Err(e) => error!("Telegram dispatcher panicked: {e}, restarting in 5s..."),
                }
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
    let status = poll_printer(state).await?;

    if !state.monitor.lock().await.monitoring_enabled {
        return Ok(());
    }

    let job = status.as_ref().and_then(|s| s.job.as_ref());
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

/// Poll printer and update state. Handles non-printing transitions
/// (reset detection, notify on stop). Returns `None` if not printing.
async fn poll_printer(state: &AppState) -> Result<Option<StatusResponse>, MonitorError> {
    let Some(prusa) = &state.prusa else {
        return Ok(None);
    };

    let status = prusa.status().await?;
    let current = status.printer.state;
    info!(state = ?current, "Printer status");

    let mut mon = state.monitor.lock().await;
    let was_printing = mon.printer_state == PrinterState::Printing;
    mon.printer_state = current;

    if current != PrinterState::Printing {
        mon.detection.reset_short_term();
        mon.alert_level = AlertLevel::Safe;
        drop(mon);
        if was_printing {
            notify_print_stopped(state, current).await?;
        }
        return Ok(None);
    }

    Ok(Some(status))
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

/// Check alert escalation against current level.
/// Returns `None` if safe or no escalation (already notified at this level).
fn check_escalation(
    mon: &mut MonitorState,
    result: DetectionResult,
) -> Option<(AlertLevel, f64, bool)> {
    let (alert, score) = match result {
        DetectionResult::Safe => {
            mon.alert_level = AlertLevel::Safe;
            return None;
        }
        DetectionResult::Warning { score } => (AlertLevel::Warning, score),
        DetectionResult::Failing { score } => (AlertLevel::Failing, score),
    };

    if alert <= mon.alert_level {
        return None;
    }

    mon.alert_level = alert;
    Some((alert, score, mon.auto_pause))
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
    let escalation = check_escalation(&mut mon, result);

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
        Command::Stealth(ref arg) | Command::Monitor(ref arg) | Command::Autopause(ref arg) => {
            let Some(enable) = parse_toggle(arg) else {
                let name = match &cmd {
                    Command::Stealth(_) => "stealth",
                    Command::Monitor(_) => "monitor",
                    Command::Autopause(_) => "autopause",
                    _ => unreachable!(),
                };
                bot.send_message(chat, format!("Usage: /{name} [on|off|1|0|true|false]"))
                    .await?;
                return Ok(());
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
        assert_eq!(parse_toggle("on"), Some(Some(true)));
        assert_eq!(parse_toggle("1"), Some(Some(true)));
        assert_eq!(parse_toggle("true"), Some(Some(true)));
        assert_eq!(parse_toggle("ON"), Some(Some(true)));
        assert_eq!(parse_toggle("True"), Some(Some(true)));
        assert_eq!(parse_toggle("  on  "), Some(Some(true)));
    }

    #[test]
    fn parse_toggle_off_variants() {
        assert_eq!(parse_toggle("off"), Some(Some(false)));
        assert_eq!(parse_toggle("0"), Some(Some(false)));
        assert_eq!(parse_toggle("false"), Some(Some(false)));
        assert_eq!(parse_toggle("OFF"), Some(Some(false)));
    }

    #[test]
    fn parse_toggle_empty_returns_none_value() {
        assert_eq!(parse_toggle(""), Some(None));
        assert_eq!(parse_toggle("  "), Some(None));
    }

    #[test]
    fn parse_toggle_invalid_returns_none() {
        assert_eq!(parse_toggle("yes"), None);
        assert_eq!(parse_toggle("no"), None);
        assert_eq!(parse_toggle("2"), None);
        assert_eq!(parse_toggle("maybe"), None);
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

    fn mon(monitoring_enabled: bool, auto_pause: bool) -> MonitorState {
        MonitorState {
            detection: DetectionState::new(1.0),
            printer_state: PrinterState::Idle,
            alert_level: AlertLevel::Safe,
            monitoring_enabled,
            auto_pause,
        }
    }

    #[test]
    fn escalation_safe_resets_alert_level() {
        let mut m = mon(true, true);
        m.alert_level = AlertLevel::Warning;
        let result = check_escalation(&mut m, DetectionResult::Safe);
        assert!(result.is_none());
        assert_eq!(m.alert_level, AlertLevel::Safe);
    }

    #[test]
    fn escalation_safe_to_warning() {
        let mut m = mon(true, true);
        let result = check_escalation(&mut m, DetectionResult::Warning { score: 0.4 });
        assert!(result.is_some());
        let (alert, score, _) = result.unwrap();
        assert_eq!(alert, AlertLevel::Warning);
        assert_eq!(score, 0.4);
        assert_eq!(m.alert_level, AlertLevel::Warning);
    }

    #[test]
    fn escalation_warning_to_failing() {
        let mut m = mon(true, true);
        m.alert_level = AlertLevel::Warning;
        let result = check_escalation(&mut m, DetectionResult::Failing { score: 0.7 });
        assert!(result.is_some());
        let (alert, _, _) = result.unwrap();
        assert_eq!(alert, AlertLevel::Failing);
        assert_eq!(m.alert_level, AlertLevel::Failing);
    }

    #[test]
    fn escalation_suppressed_at_same_level() {
        let mut m = mon(true, true);
        m.alert_level = AlertLevel::Warning;
        let result = check_escalation(&mut m, DetectionResult::Warning { score: 0.5 });
        assert!(result.is_none());
        // alert_level unchanged
        assert_eq!(m.alert_level, AlertLevel::Warning);
    }

    #[test]
    fn escalation_suppressed_at_higher_level() {
        let mut m = mon(true, true);
        m.alert_level = AlertLevel::Failing;
        let result = check_escalation(&mut m, DetectionResult::Warning { score: 0.4 });
        assert!(result.is_none());
        // stays at Failing, not downgraded
        assert_eq!(m.alert_level, AlertLevel::Failing);
    }

    #[test]
    fn escalation_returns_auto_pause_setting() {
        let mut m = mon(true, false);
        let result = check_escalation(&mut m, DetectionResult::Failing { score: 0.8 });
        let (_, _, auto_pause) = result.unwrap();
        assert!(!auto_pause);

        let mut m = mon(true, true);
        let result = check_escalation(&mut m, DetectionResult::Failing { score: 0.8 });
        let (_, _, auto_pause) = result.unwrap();
        assert!(auto_pause);
    }

    #[test]
    fn should_pause_only_on_failing_with_auto_pause() {
        assert!(should_pause(AlertLevel::Failing, true));
        assert!(!should_pause(AlertLevel::Failing, false));
        assert!(!should_pause(AlertLevel::Warning, true));
        assert!(!should_pause(AlertLevel::Warning, false));
        assert!(!should_pause(AlertLevel::Safe, true));
    }

    #[test]
    fn full_flow_safe_to_warning_to_failing() {
        let mut m = mon(true, true);

        // First: safe
        let result = check_escalation(&mut m, DetectionResult::Safe);
        assert!(result.is_none());
        assert_eq!(m.alert_level, AlertLevel::Safe);

        // Escalate to warning
        let result = check_escalation(&mut m, DetectionResult::Warning { score: 0.4 });
        assert!(result.is_some());
        assert_eq!(m.alert_level, AlertLevel::Warning);

        // Repeated warning suppressed
        let result = check_escalation(&mut m, DetectionResult::Warning { score: 0.45 });
        assert!(result.is_none());

        // Escalate to failing
        let result = check_escalation(&mut m, DetectionResult::Failing { score: 0.7 });
        assert!(result.is_some());
        assert_eq!(m.alert_level, AlertLevel::Failing);
        let (_, _, auto_pause) = result.unwrap();
        assert!(should_pause(AlertLevel::Failing, auto_pause));

        // Repeated failing suppressed
        let result = check_escalation(&mut m, DetectionResult::Failing { score: 0.8 });
        assert!(result.is_none());

        // Back to safe resets
        let result = check_escalation(&mut m, DetectionResult::Safe);
        assert!(result.is_none());
        assert_eq!(m.alert_level, AlertLevel::Safe);

        // Can escalate again after reset
        let result = check_escalation(&mut m, DetectionResult::Warning { score: 0.35 });
        assert!(result.is_some());
    }

    #[test]
    fn full_flow_auto_pause_off_still_alerts_but_no_pause() {
        let mut m = mon(true, false);

        let result = check_escalation(&mut m, DetectionResult::Failing { score: 0.8 });
        assert!(result.is_some());
        let (alert, _, auto_pause) = result.unwrap();
        assert_eq!(alert, AlertLevel::Failing);
        assert!(!should_pause(alert, auto_pause));
    }
}
