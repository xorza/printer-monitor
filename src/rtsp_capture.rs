use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use openh264::formats::YUVSource;
use retina::client::{PlayOptions, Session, SessionOptions, SetupOptions, Transport};
use retina::codec::{CodecItem, FrameFormat};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

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

    pub async fn capture(&self, output_path: &Path) -> Result<(), CaptureError> {
        tokio::time::timeout(CAPTURE_TIMEOUT, self.capture_inner(output_path))
            .await
            .map_err(|_| CaptureError::Timeout)?
    }

    async fn capture_inner(&self, output_path: &Path) -> Result<(), CaptureError> {
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
        let result = self.read_first_frame(&mut demuxed, output_path).await;

        drop(demuxed);
        let _ = session_group.await_teardown().await;

        result
    }

    async fn read_first_frame(
        &self,
        demuxed: &mut retina::client::Demuxed,
        output_path: &Path,
    ) -> Result<(), CaptureError> {
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

            let file = std::fs::File::create(output_path)?;
            let encoder = jpeg_encoder::Encoder::new(file, 85);
            encoder.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)?;

            return Ok(());
        }

        Err(CaptureError::NoFrame)
    }
}

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
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("JPEG encode: {0}")]
    Jpeg(#[from] jpeg_encoder::EncodingError),
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
    async fn capture_live_frame() {
        let cap = RtspCapture::new("rtsp://prusacam.lan/live");
        let path = Path::new("./rtsp_capture_test.jpg");
        cap.capture(path).await.unwrap();

        let metadata = std::fs::metadata(path).unwrap();
        assert!(
            metadata.len() > 1000,
            "JPEG too small: {} bytes",
            metadata.len()
        );

        //std::fs::remove_file(path).ok();
    }
}
