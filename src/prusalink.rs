use reqwest::Client;
use serde::Deserialize;

#[derive(Debug)]
pub struct PrusaLink {
    client: Client,
    base_url: String,
    api_key: String,
}

#[derive(Debug, Deserialize)]
pub struct StatusResponse {
    pub printer: PrinterStatus,
    pub job: Option<JobStatus>,
}

#[derive(Debug, Deserialize)]
pub struct PrinterStatus {
    pub state: PrinterState,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PrinterState {
    Idle,
    Busy,
    Printing,
    Paused,
    Finished,
    Stopped,
    Error,
    Attention,
    Ready,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct JobStatus {
    pub id: u64,
    pub progress: Option<f64>,
    pub time_remaining: Option<u64>,
    pub time_printing: Option<u64>,
}

impl PrusaLink {
    pub fn new(client: Client, base_url: String, api_key: String) -> Self {
        Self {
            client,
            base_url,
            api_key,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .get(self.url(path))
            .header("X-Api-Key", &self.api_key)
    }

    fn put(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .put(self.url(path))
            .header("X-Api-Key", &self.api_key)
    }

    pub async fn status(&self) -> Result<StatusResponse, reqwest::Error> {
        self.get("/api/v1/status")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    pub async fn pause(&self, job_id: u64) -> Result<(), reqwest::Error> {
        self.put(&format!("/api/v1/job/{job_id}/pause"))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn resume(&self, job_id: u64) -> Result<(), reqwest::Error> {
        self.put(&format!("/api/v1/job/{job_id}/resume"))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_status_printing() {
        let json = r#"{
            "printer": { "state": "PRINTING" },
            "job": { "id": 42, "progress": 55.3, "time_remaining": 600, "time_printing": 300 }
        }"#;
        let status: StatusResponse = serde_json::from_str(json).unwrap();
        assert_eq!(status.printer.state, PrinterState::Printing);
        let job = status.job.unwrap();
        assert_eq!(job.id, 42);
        assert_eq!(job.progress.unwrap(), 55.3);
        assert_eq!(job.time_remaining.unwrap(), 600);
        assert_eq!(job.time_printing.unwrap(), 300);
    }

    #[test]
    fn deserialize_status_idle_no_job() {
        let json = r#"{
            "printer": { "state": "IDLE" }
        }"#;
        let status: StatusResponse = serde_json::from_str(json).unwrap();
        assert_eq!(status.printer.state, PrinterState::Idle);
        assert!(status.job.is_none());
    }

    #[test]
    fn deserialize_status_paused() {
        let json = r#"{
            "printer": { "state": "PAUSED" },
            "job": { "id": 7 }
        }"#;
        let status: StatusResponse = serde_json::from_str(json).unwrap();
        assert_eq!(status.printer.state, PrinterState::Paused);
        let job = status.job.unwrap();
        assert_eq!(job.id, 7);
        // progress/time fields are optional
        assert!(job.progress.is_none());
        assert!(job.time_remaining.is_none());
    }

    #[test]
    fn deserialize_all_printer_states() {
        for (state_str, expected) in [
            ("IDLE", PrinterState::Idle),
            ("BUSY", PrinterState::Busy),
            ("PRINTING", PrinterState::Printing),
            ("PAUSED", PrinterState::Paused),
            ("FINISHED", PrinterState::Finished),
            ("STOPPED", PrinterState::Stopped),
            ("ERROR", PrinterState::Error),
            ("ATTENTION", PrinterState::Attention),
            ("READY", PrinterState::Ready),
        ] {
            let json = format!(r#"{{"printer": {{"state": "{}"}}}}"#, state_str);
            let status: StatusResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(status.printer.state, expected);
        }
    }

    #[test]
    fn deserialize_unknown_state() {
        let json = r#"{"printer": {"state": "CALIBRATING"}}"#;
        let status: StatusResponse = serde_json::from_str(json).unwrap();
        assert_eq!(status.printer.state, PrinterState::Unknown);
    }
}
