pub mod config;
pub mod detection;
pub mod obico;
pub mod prusalink;
pub mod server;
pub mod snapshot;
pub mod telegram;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use detection::{DetectionResult, DetectionState};
use prusalink::{JobStatus, PrusaLink};
use server::ImageServer;
use teloxide::dispatching::HandlerExt;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, InlineKeyboardButton, InputFile};
use teloxide::utils::command::BotCommands;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

#[derive(Debug, thiserror::Error)]
enum MonitorError {
    #[error("PrusaLink: {0}")]
    PrusaLink(#[from] reqwest::Error),
    #[error("Snapshot: {0}")]
    Snapshot(#[from] std::io::Error),
    #[error("Obico: {0}")]
    Obico(#[from] obico::ObicoError),
    #[error("Telegram: {0}")]
    Telegram(#[from] teloxide::RequestError),
}

const POLL_INTERVAL: Duration = Duration::from_secs(10);
const MONITOR_SNAPSHOT_PATH: &str = "/tmp/snapshot_monitor.jpg";
const PHOTO_SNAPSHOT_PATH: &str = "/tmp/snapshot_photo.jpg";

#[derive(Debug, Clone)]
struct AppState {
    prusa: Option<Arc<PrusaLink>>,
    obico: Arc<obico::Obico>,
    tg: Arc<telegram::Telegram>,
    snapshot: Arc<snapshot::Snapshot>,
    image_server: Arc<ImageServer>,
    detection: Arc<Mutex<DetectionState>>,
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
}

// --- Entry points ---

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
        snapshot: Arc::new(snapshot::Snapshot::new(config.rtsp_url)),
        image_server: Arc::new(image_server),
        detection: Arc::new(Mutex::new(DetectionState::new(
            config.detection_sensitivity,
        ))),
    };

    // Shutdown on SIGTERM (Docker) or Ctrl+C
    tokio::spawn({
        let token = token.clone();
        async move {
            let ctrl_c = tokio::signal::ctrl_c();
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            tokio::select! {
                _ = ctrl_c => info!("Received SIGINT, shutting down..."),
                _ = sigterm.recv() => info!("Received SIGTERM, shutting down..."),
            }
            token.cancel();
        }
    });

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
    let mut dispatcher = Dispatcher::builder(state.tg.bot().clone(), handler)
        .dependencies(dptree::deps![state.clone()])
        .build();
    let shutdown_token = dispatcher.shutdown_token();

    tokio::spawn({
        let token = token.clone();
        async move {
            loop {
                dispatcher.dispatch().await;
                if token.is_cancelled() {
                    break;
                }
                error!("Telegram dispatcher stopped, restarting in 5s...");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    });

    loop {
        if let Err(e) = monitor_cycle(&state).await {
            error!("Monitor error: {e}");
        }
        tokio::select! {
            _ = token.cancelled() => break,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }

    match shutdown_token.shutdown() {
        Ok(fut) => fut.await,
        Err(e) => error!("Bot shutdown error: {e:?}"),
    }

    info!("Shutdown complete.");
}

// --- Monitor loop ---

async fn monitor_cycle(state: &AppState) -> Result<(), MonitorError> {
    let status = if let Some(prusa) = &state.prusa {
        let s = prusa.status().await?;
        info!(state = ?s.printer.state, "Printer status");
        if s.printer.state != prusalink::PrinterState::Printing {
            state.detection.lock().await.reset_short_term();
            return Ok(());
        }
        Some(s)
    } else {
        None
    };

    let job = status.as_ref().and_then(|s| s.job.as_ref());
    if let Some(job) = job {
        info!(job_id = job.id, "{}", format_job_info(job));
    }

    let path = Path::new(MONITOR_SNAPSHOT_PATH);
    state.snapshot.capture(path).await?;

    let image_data = tokio::fs::read(path).await?;
    state.image_server.set_image(image_data);

    let obico_result = state.obico.detect().await?;

    let result = state
        .detection
        .lock()
        .await
        .update(&obico_result.detections, job.map(|j| j.id));

    let (adjusted, paused) = match result {
        DetectionResult::Safe => {
            info!("Detection: safe");
            return Ok(());
        }
        DetectionResult::Warning { score: adjusted } => {
            warn!(adjusted, "Detection: warning");
            (adjusted, false)
        }
        DetectionResult::Failing { score: adjusted } => {
            warn!(adjusted, "Detection: failing");
            let paused = if let Some((prusa, job)) = state.prusa.as_ref().zip(job) {
                prusa.pause(job.id).await?;
                true
            } else {
                false
            };
            (adjusted, paused)
        }
    };

    let action = if paused {
        "Print has been paused."
    } else {
        "Print is still running — monitor closely."
    };
    let job_line = job
        .map(|j| format!("{}\n", format_job_info(j)))
        .unwrap_or_default();
    let caption = format!("Print failure detected!\n{job_line}Score: {adjusted:.2}\n{action}");

    let buttons = match (paused, state.prusa.is_some()) {
        (true, _) => vec![InlineKeyboardButton::callback("Resume", "resume")],
        (false, true) => vec![InlineKeyboardButton::callback("Pause", "pause")],
        (false, false) => vec![],
    };
    state.tg.send_photo(path, &caption, &buttons).await?;

    Ok(())
}

// --- Bot commands ---

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
            let path = Path::new(PHOTO_SNAPSHOT_PATH);
            let (caption, snapshot) =
                tokio::join!(status_caption(&state), state.snapshot.capture(path));
            match snapshot {
                Ok(_) => {
                    bot.send_photo(chat, InputFile::file(path))
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
    }
    Ok(())
}

async fn handle_callback(
    bot: Bot,
    q: CallbackQuery,
    state: AppState,
) -> Result<(), teloxide::RequestError> {
    let data = q.data.as_deref().unwrap_or("");
    let pause = match data {
        "pause" => true,
        "resume" => false,
        _ => {
            bot.answer_callback_query(q.id).await?;
            return Ok(());
        }
    };

    let reply = match &state.prusa {
        Some(prusa) => handle_pause_resume(prusa, pause).await,
        None => "PrusaLink not configured.".to_string(),
    };

    // Remove the inline keyboard after action
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

async fn status_caption(state: &AppState) -> String {
    let score = state.detection.lock().await.current_score();
    let score_line = format!("Detection score: {score:.2}");

    let Some(prusa) = state.prusa.as_ref() else {
        return format!("PrusaLink not configured.\n{score_line}");
    };
    match prusa.status().await {
        Ok(status) => {
            let job_info = status
                .job
                .as_ref()
                .map(format_job_info)
                .unwrap_or_else(|| "No active job".to_string());
            format!(
                "State: {:?}\n{job_info}\n{score_line}",
                status.printer.state
            )
        }
        Err(e) => {
            error!("PrusaLink status error: {e}");
            format!("PrusaLink error: {e}\n{score_line}")
        }
    }
}

// --- Helpers ---

fn format_job_info(job: &JobStatus) -> String {
    format!(
        "Job #{}, progress: {:.1}%",
        job.id,
        job.progress.unwrap_or(0.0)
    )
}
