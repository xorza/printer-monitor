use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::schedule::StealthSchedule;

const DEFAULT_PATH: &str = "settings.toml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub monitoring_enabled: bool,
    pub auto_pause: bool,
    #[serde(default)]
    pub stealth_schedule: StealthSchedule,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            monitoring_enabled: true,
            auto_pause: true,
            stealth_schedule: StealthSchedule::default(),
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        Self::load_from(Path::new(DEFAULT_PATH))
    }

    pub fn save(&self) {
        self.save_to(Path::new(DEFAULT_PATH));
    }

    fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(settings) => {
                    info!("Loaded settings from {}", path.display());
                    settings
                }
                Err(e) => {
                    error!("Failed to parse {}: {e}, using defaults", path.display());
                    Self::default()
                }
            },
            Err(_) => {
                info!("No settings file found, using defaults");
                let settings = Self::default();
                settings.save_to(path);
                settings
            }
        }
    }

    fn save_to(&self, path: &Path) {
        let contents = toml::to_string_pretty(self).unwrap();
        if let Err(e) = std::fs::write(path, contents) {
            error!("Failed to save settings to {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn default_values() {
        let s = Settings::default();
        assert!(s.monitoring_enabled);
        assert!(s.auto_pause);
    }

    #[test]
    fn roundtrip() {
        let s = Settings {
            monitoring_enabled: false,
            auto_pause: true,
            stealth_schedule: StealthSchedule {
                enabled: true,
                off_at: "07:30".to_string(),
                on_at: "21:15".to_string(),
            },
        };
        let toml = toml::to_string_pretty(&s).unwrap();
        let loaded: Settings = toml::from_str(&toml).unwrap();
        assert_eq!(s, loaded);
    }

    #[test]
    fn load_old_format_without_schedule_gets_default() {
        // Existing deployments have settings.toml without stealth_schedule — must not crash.
        let toml_text = "monitoring_enabled = true\nauto_pause = false\n";
        let loaded: Settings = toml::from_str(toml_text).unwrap();
        assert!(loaded.monitoring_enabled);
        assert!(!loaded.auto_pause);
        assert_eq!(loaded.stealth_schedule, StealthSchedule::default());
    }

    #[test]
    fn load_missing_file_returns_default() {
        let s = Settings::load_from(Path::new("/tmp/nonexistent_prusa_settings.toml"));
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn load_invalid_toml_returns_default() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "not valid {{ toml").unwrap();
        let s = Settings::load_from(f.path());
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_settings.toml");

        let s = Settings {
            monitoring_enabled: false,
            auto_pause: false,
            stealth_schedule: StealthSchedule {
                enabled: true,
                off_at: "09:00".to_string(),
                on_at: "22:00".to_string(),
            },
        };
        s.save_to(&path);
        let loaded = Settings::load_from(&path);
        assert_eq!(s, loaded);
    }
}
