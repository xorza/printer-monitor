use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub struct Snapshot {
    rtsp_url: String,
}

impl Snapshot {
    pub fn new(rtsp_url: String) -> Self {
        assert!(
            rtsp_url.starts_with("rtsp://") || rtsp_url.starts_with("rtsps://"),
            "RTSP_URL must start with rtsp:// or rtsps://, got: {rtsp_url}"
        );
        Self { rtsp_url }
    }

    pub async fn capture(&self, output_path: &Path) -> Result<(), std::io::Error> {
        let timeout_us = CAPTURE_TIMEOUT.as_micros().to_string();
        let child = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-rtsp_transport",
                "tcp",
                "-timeout",
                &timeout_us,
                "-i",
                &self.rtsp_url,
                "-frames:v",
                "1",
                "-q:v",
                "2",
            ])
            .arg(output_path)
            .kill_on_drop(true)
            .spawn()?;

        let output = match tokio::time::timeout(CAPTURE_TIMEOUT, child.wait_with_output()).await {
            Ok(result) => result?,
            // kill_on_drop(true) ensures the child is killed when dropped here
            Err(_) => return Err(std::io::Error::other("ffmpeg timed out")),
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(std::io::Error::other(format!("ffmpeg failed: {stderr}")));
        }

        Ok(())
    }
}
