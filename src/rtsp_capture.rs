use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use openh264::formats::YUVSource;
use retina::client::{PlayOptions, Session, SessionOptions, SetupOptions, Transport};
use retina::codec::{CodecItem, FrameFormat};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
const JPEG_QUALITY: u8 = 85;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("timed out")]
    Timeout,
    #[error("no H.264 video stream found")]
    NoVideoStream,
    #[error("stream ended without producing a frame")]
    NoFrame,
    #[error("RTSP: {0}")]
    Rtsp(#[from] retina::Error),
    #[error("H.264 decode: {0}")]
    Decode(#[from] openh264::Error),
    #[error("JPEG encode: {0}")]
    Jpeg(#[from] jpeg_encoder::EncodingError),
}

#[derive(Debug)]
pub struct RtspCapture {
    url: url::Url,
}

impl RtspCapture {
    pub fn new(rtsp_url: &str) -> Self {
        let url = url::Url::parse(rtsp_url).unwrap_or_else(|e| panic!("Invalid RTSP URL: {e}"));
        assert!(
            url.scheme() == "rtsp" || url.scheme() == "rtsps",
            "URL must use rtsp:// or rtsps://, got: {rtsp_url}"
        );
        Self { url }
    }

    /// Capture a single frame and return JPEG bytes.
    pub async fn capture(&self) -> Result<Vec<u8>, CaptureError> {
        tokio::time::timeout(CAPTURE_TIMEOUT, self.capture_inner())
            .await
            .map_err(|_| CaptureError::Timeout)?
    }

    async fn capture_inner(&self) -> Result<Vec<u8>, CaptureError> {
        let session_group = Arc::new(retina::client::SessionGroup::default());
        let options = SessionOptions::default()
            .session_group(session_group.clone())
            .user_agent("printer-monitor".to_owned());

        let mut session = Session::describe(self.url.clone(), options).await?;

        let video_i = session
            .streams()
            .iter()
            .position(|s| s.media() == "video" && s.encoding_name() == "h264")
            .ok_or(CaptureError::NoVideoStream)?;

        session
            .setup(
                video_i,
                SetupOptions::default()
                    .transport(Transport::Tcp(Default::default()))
                    .frame_format(FrameFormat::SIMPLE),
            )
            .await?;

        let mut demuxed = session.play(PlayOptions::default()).await?.demuxed()?;
        let result = decode_first_frame(&mut demuxed).await;

        drop(demuxed);
        let _ = session_group.await_teardown().await;

        result
    }
}

async fn decode_first_frame(
    demuxed: &mut retina::client::Demuxed,
) -> Result<Vec<u8>, CaptureError> {
    let mut decoder = openh264::decoder::Decoder::new()?;
    let mut got_keyframe = false;

    while let Some(item) = demuxed.next().await {
        let frame = match item? {
            CodecItem::VideoFrame(f) => f,
            _ => continue,
        };

        if frame.is_random_access_point() {
            got_keyframe = true;
        }
        if !got_keyframe {
            continue;
        }

        let yuv = match decoder.decode(frame.data()) {
            Ok(Some(yuv)) => yuv,
            Ok(None) => continue,
            Err(_) if !frame.is_random_access_point() => continue,
            Err(e) => return Err(e.into()),
        };

        let (w, h) = yuv.dimensions();
        let mut rgb = vec![0u8; w * h * 3];
        yuv.write_rgb8(&mut rgb);

        let mut jpeg = Vec::new();
        let encoder = jpeg_encoder::Encoder::new(&mut jpeg, JPEG_QUALITY);
        encoder.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)?;

        return Ok(jpeg);
    }

    Err(CaptureError::NoFrame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_url() {
        let cap = RtspCapture::new("rtsp://192.168.0.10/live");
        assert_eq!(cap.url.scheme(), "rtsp");
    }

    #[test]
    #[should_panic(expected = "rtsp://")]
    fn rejects_http_url() {
        RtspCapture::new("http://192.168.0.10/live");
    }

    #[test]
    #[should_panic(expected = "Invalid RTSP URL")]
    fn rejects_garbage() {
        RtspCapture::new("not a url");
    }

    #[tokio::test]
    #[ignore] // requires live camera at prusacam.lan
    async fn capture_to_memory() {
        let cap = RtspCapture::new("rtsp://prusacam.lan/live");
        let jpeg = cap.capture().await.unwrap();
        assert!(jpeg.len() > 1000, "JPEG too small: {} bytes", jpeg.len());
    }
}
