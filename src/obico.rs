use reqwest::Client;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ObicoError {
    #[error("{0}")]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Api(String),
}

#[derive(Debug)]
pub struct Obico {
    client: Client,
    detect_url: String,
}

#[derive(Debug, Deserialize)]
pub struct DetectionResponse {
    pub detections: Vec<Detection>,
    pub message: Option<String>,
}

#[derive(Debug)]
pub struct Detection {
    pub label: String,
    pub confidence: f64,
    pub bbox: [f64; 4],
}

impl<'de> Deserialize<'de> for Detection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // API returns: ["failure", 0.85, [120.5, 200.3, 50.0, 80.0]]
        let (label, confidence, bbox): (String, f64, [f64; 4]) =
            Deserialize::deserialize(deserializer)?;
        Ok(Detection {
            label,
            confidence,
            bbox,
        })
    }
}

impl Obico {
    pub fn new(client: Client, api_url: &str, image_host: &str) -> Self {
        Self {
            client,
            detect_url: format!("{api_url}/p/?img=http://{image_host}/snapshot.jpg"),
        }
    }

    pub async fn detect(&self) -> Result<DetectionResponse, ObicoError> {
        let resp = self
            .client
            .get(&self.detect_url)
            .send()
            .await?
            .error_for_status()?
            .json::<DetectionResponse>()
            .await?;
        if let Some(msg) = &resp.message {
            return Err(ObicoError::Api(msg.clone()));
        }
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_empty_detections() {
        let json = r#"{"detections": []}"#;
        let resp: DetectionResponse = serde_json::from_str(json).unwrap();
        assert!(resp.detections.is_empty());
        assert!(resp.message.is_none());
    }

    #[test]
    fn deserialize_detections_with_results() {
        let json = r#"{
            "detections": [
                ["failure", 0.85, [120.5, 200.3, 50.0, 80.0]],
                ["failure", 0.42, [300.1, 150.7, 60.0, 45.0]]
            ]
        }"#;
        let resp: DetectionResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.detections.len(), 2);

        assert_eq!(resp.detections[0].label, "failure");
        assert_eq!(resp.detections[0].confidence, 0.85);
        assert_eq!(resp.detections[0].bbox, [120.5, 200.3, 50.0, 80.0]);

        assert_eq!(resp.detections[1].label, "failure");
        assert_eq!(resp.detections[1].confidence, 0.42);
        assert_eq!(resp.detections[1].bbox, [300.1, 150.7, 60.0, 45.0]);
    }

    #[test]
    fn deserialize_detection_with_error_message() {
        let json = r#"{"detections": [], "message": "some error"}"#;
        let resp: DetectionResponse = serde_json::from_str(json).unwrap();
        assert!(resp.detections.is_empty());
        assert_eq!(resp.message.unwrap(), "some error");
    }
}
