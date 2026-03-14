use teloxide::types::ChatId;

#[derive(Debug)]
pub struct PrusaLinkConfig {
    pub url: String,
    pub api_key: String,
}

#[derive(Debug)]
pub struct Config {
    pub prusalink: Option<PrusaLinkConfig>,
    pub rtsp_url: String,
    pub obico_url: String,
    pub obico_image_host: String,
    pub telegram_bot_token: String,
    pub telegram_chat_id: ChatId,
    pub detection_sensitivity: f64,
}

impl Config {
    pub fn from_env() -> Self {
        fn env(name: &str) -> String {
            std::env::var(name).unwrap_or_else(|_| panic!("{name} env var must be set"))
        }

        let chat_id: i64 = env("TELEGRAM_CHAT_ID")
            .parse()
            .expect("TELEGRAM_CHAT_ID must be a number");

        let detection_sensitivity: f64 = std::env::var("DETECTION_SENSITIVITY")
            .unwrap_or_else(|_| "1.0".to_string())
            .parse()
            .expect("DETECTION_SENSITIVITY must be a number");
        assert!(
            (0.1..=5.0).contains(&detection_sensitivity),
            "DETECTION_SENSITIVITY must be between 0.1 and 5.0, got: {detection_sensitivity}"
        );

        let prusalink = match (
            std::env::var("PRUSALINK_URL").ok(),
            std::env::var("PRUSALINK_API_KEY").ok(),
        ) {
            (Some(url), Some(api_key)) => Some(PrusaLinkConfig { url, api_key }),
            (None, None) => None,
            _ => panic!("PRUSALINK_URL and PRUSALINK_API_KEY must both be set or both be unset"),
        };

        Self {
            prusalink,
            rtsp_url: env("RTSP_URL"),
            obico_url: env("OBICO_URL"),
            obico_image_host: {
                let host = env("OBICO_IMAGE_HOST");
                assert!(
                    host.rsplit_once(':').is_some(),
                    "OBICO_IMAGE_HOST must be host:port, got: {host}"
                );
                host
            },
            telegram_bot_token: env("TELEGRAM_BOT_TOKEN"),
            telegram_chat_id: ChatId(chat_id),
            detection_sensitivity,
        }
    }
}
