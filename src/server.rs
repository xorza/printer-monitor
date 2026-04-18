use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use bytes::Bytes;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::error;

#[derive(Debug, Clone)]
pub struct ImageServer {
    image_data: Arc<RwLock<Option<Bytes>>>,
    addr: SocketAddr,
}

impl ImageServer {
    pub async fn start(image_host: &str, cancel: CancellationToken) -> Self {
        // image_host is host:port (validated in Config::from_env); bind to 0.0.0.0 on that port
        let port = image_host.rsplit_once(':').unwrap().1;
        Self::bind(&format!("0.0.0.0:{port}"), cancel).await
    }

    async fn bind(bind_addr: &str, cancel: CancellationToken) -> Self {
        let image_data: Arc<RwLock<Option<Bytes>>> = Arc::new(RwLock::new(None));

        let app = Router::new()
            .route("/snapshot.jpg", get(serve_image))
            .route("/health", get(|| async { StatusCode::OK }))
            .with_state(image_data.clone());

        let listener = TcpListener::bind(&bind_addr)
            .await
            .expect("failed to bind image server");
        let addr = listener.local_addr().unwrap();

        // If the server crashes (or the spawned task panics), trip the
        // cancellation token so the whole app shuts down — the monitor loop
        // depends on the image server to serve snapshots to Obico.
        let fail_cancel = cancel.clone();
        tokio::spawn(async move {
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(async move { cancel.cancelled().await })
                .await;
            if let Err(e) = result {
                error!("Image server crashed: {e}");
            }
            fail_cancel.cancel();
        });

        Self { image_data, addr }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Updates the image served at /snapshot.jpg
    pub fn set_image(&self, data: Vec<u8>) {
        *self.image_data.write().unwrap() = Some(Bytes::from(data));
    }
}

async fn serve_image(State(image_data): State<Arc<RwLock<Option<Bytes>>>>) -> impl IntoResponse {
    match image_data.read().unwrap().clone() {
        Some(data) => {
            (StatusCode::OK, [(header::CONTENT_TYPE, "image/jpeg")], data).into_response()
        }
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cancel() -> CancellationToken {
        CancellationToken::new()
    }

    #[tokio::test]
    async fn serves_image_data() {
        let server = ImageServer::bind("127.0.0.1:0", test_cancel()).await;
        let test_data = vec![0xFF, 0xD8, 0xFF, 0xE0]; // JPEG magic bytes
        server.set_image(test_data.clone());

        let url = format!("http://{}/snapshot.jpg", server.addr());
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 200);

        let body = resp.bytes().await.unwrap();
        assert_eq!(body.as_ref(), &test_data);
    }

    #[tokio::test]
    async fn returns_503_before_first_snapshot() {
        let server = ImageServer::bind("127.0.0.1:0", test_cancel()).await;

        let url = format!("http://{}/snapshot.jpg", server.addr());
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 503);
    }

    #[tokio::test]
    async fn updates_image() {
        let server = ImageServer::bind("127.0.0.1:0", test_cancel()).await;
        let url = format!("http://{}/snapshot.jpg", server.addr());

        server.set_image(vec![1, 2, 3]);
        let body = reqwest::get(&url).await.unwrap().bytes().await.unwrap();
        assert_eq!(body.as_ref(), &[1, 2, 3]);

        server.set_image(vec![4, 5, 6, 7]);
        let body = reqwest::get(&url).await.unwrap().bytes().await.unwrap();
        assert_eq!(body.as_ref(), &[4, 5, 6, 7]);
    }
}
