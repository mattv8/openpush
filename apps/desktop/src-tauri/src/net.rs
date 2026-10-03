//! Authenticated, origin-bound HTTP. Redirects are disabled, TLS is mandatory except for
//! explicit loopback development origins, every response body is read incrementally against a
//! hard cap (Content-Length is only an early rejection hint), and media is streamed to/from
//! files instead of being buffered.
use crate::{error::BridgeError, origin::is_loopback_http};
use futures_util::StreamExt;
use reqwest::{header, redirect, RequestBuilder, Response, StatusCode};
use serde::{de::DeserializeOwned, Serialize};
use std::{path::Path, time::Duration};
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

pub const MAX_JSON_BYTES: usize = 1024 * 1024;
/// History/snapshot pages: same bound core applies to a snapshot page.
pub const MAX_PAGE_BYTES: usize = peppy_client_core::MAX_SNAPSHOT_PAGE_BYTES;
const MAX_ERROR_BODY_BYTES: usize = 4 * 1024;
const TRANSFER_CHUNK: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetError {
    Offline,
    Revoked,
    Status { status: u16, code: Option<String> },
    TooLarge,
    Invalid,
    Local,
}

impl NetError {
    pub fn is_status(&self, status: u16, code: &str) -> bool {
        matches!(self, NetError::Status { status: s, code: Some(c) } if *s == status && c == code)
    }
}

impl From<NetError> for BridgeError {
    fn from(error: NetError) -> Self {
        match error {
            NetError::Offline => {
                BridgeError::new("offline", "Could not reach the configured server.")
            }
            NetError::Revoked => BridgeError::new(
                "revoked",
                "The server rejected this device credential (revoked or invalid).",
            ),
            NetError::Status { status, code } => BridgeError::new(
                "server-rejected",
                match code {
                    Some(code) => {
                        format!("The server rejected the request (HTTP {status}, {code}).")
                    }
                    None => format!("The server rejected the request (HTTP {status})."),
                },
            ),
            NetError::TooLarge => BridgeError::new(
                "response-too-large",
                "A server response exceeded the native safety limit.",
            ),
            NetError::Invalid => BridgeError::new(
                "server-response",
                "The server returned an invalid response.",
            ),
            NetError::Local => {
                BridgeError::new("attachment-local", "A local transfer file is unavailable.")
            }
        }
    }
}

/// Only short `snake_case` server error codes are surfaced; anything else is dropped.
fn sanitize_code(value: &serde_json::Value) -> Option<String> {
    let code = value.get("code")?.as_str()?;
    (code.len() <= 64
        && !code.is_empty()
        && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
    .then(|| code.to_owned())
}

/// Reads a response body incrementally, failing as soon as `cap` would be exceeded.
pub async fn read_bounded(mut response: Response, cap: usize) -> Result<Vec<u8>, NetError> {
    if response
        .content_length()
        .is_some_and(|length| length > cap as u64)
    {
        return Err(NetError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| NetError::Offline)? {
        if body.len() + chunk.len() > cap {
            return Err(NetError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub struct Api {
    http: reqwest::Client,
    transfer: reqwest::Client,
    origin: String,
    token: Zeroizing<String>,
}

fn builder(origin: &str) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(redirect::Policy::none())
        .https_only(!is_loopback_http(origin))
        .connect_timeout(Duration::from_secs(5))
        .user_agent("Peppy-Desktop")
}

impl Api {
    /// `origin` must already be validated (see `origin::validate_origin`).
    pub fn new(origin: &str, token: &str) -> Result<Self, BridgeError> {
        let failed = |_| BridgeError::new("network", "Could not create the native network client.");
        Ok(Self {
            http: builder(origin)
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(failed)?,
            transfer: builder(origin)
                .read_timeout(Duration::from_secs(30))
                .build()
                .map_err(failed)?,
            origin: origin.to_owned(),
            token: Zeroizing::new(token.to_owned()),
        })
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
    /// Native-only: used for the WebSocket upgrade header.
    pub fn bearer(&self) -> &str {
        &self.token
    }
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.origin, path)
    }

    async fn execute(&self, request: RequestBuilder) -> Result<Response, NetError> {
        let response = request
            .bearer_auth(self.token.as_str())
            .send()
            .await
            .map_err(|_| NetError::Offline)?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        if status == StatusCode::UNAUTHORIZED {
            return Err(NetError::Revoked);
        }
        let code = read_bounded(response, MAX_ERROR_BODY_BYTES)
            .await
            .ok()
            .and_then(|body| serde_json::from_slice::<serde_json::Value>(&body).ok())
            .and_then(|value| sanitize_code(&value));
        Err(NetError::Status {
            status: status.as_u16(),
            code,
        })
    }

    async fn json<T: DeserializeOwned>(
        &self,
        request: RequestBuilder,
        cap: usize,
    ) -> Result<T, NetError> {
        let body = read_bounded(self.execute(request).await?, cap).await?;
        serde_json::from_slice(&body).map_err(|_| NetError::Invalid)
    }

    pub async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        cap: usize,
    ) -> Result<T, NetError> {
        self.json(self.http.get(self.url(path)), cap).await
    }

    pub async fn post_json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, NetError> {
        self.json(self.http.post(self.url(path)).json(body), MAX_JSON_BYTES)
            .await
    }

    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T, NetError> {
        self.json(self.http.post(self.url(path)), MAX_JSON_BYTES)
            .await
    }

    /// Posts a body and discards a bounded response body.
    pub async fn post_discard<B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<(), NetError> {
        read_bounded(
            self.execute(self.http.post(self.url(path)).json(body))
                .await?,
            MAX_JSON_BYTES,
        )
        .await
        .map(|_| ())
    }

    /// `DELETE` with a JSON body; any 2xx is success and the bounded body is discarded.
    pub async fn delete_json<B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<(), NetError> {
        read_bounded(
            self.execute(self.http.delete(self.url(path)).json(body))
                .await?,
            MAX_JSON_BYTES,
        )
        .await
        .map(|_| ())
    }

    /// Streams exactly `length` bytes of a local file as the request body.
    pub async fn put_file(&self, path: &str, file: &Path, length: u64) -> Result<(), NetError> {
        let source = tokio::fs::File::open(file)
            .await
            .map_err(|_| NetError::Local)?;
        if source.metadata().await.map_err(|_| NetError::Local)?.len() != length {
            return Err(NetError::Local);
        }
        let stream = tokio_util::io::ReaderStream::with_capacity(
            tokio::io::AsyncReadExt::take(source, length),
            TRANSFER_CHUNK,
        );
        let request = self
            .transfer
            .put(self.url(path))
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, length)
            .body(reqwest::Body::wrap_stream(stream));
        read_bounded(self.execute(request).await?, MAX_ERROR_BODY_BYTES)
            .await
            .map(|_| ())
    }

    /// Streams a response to a new owner-only file, enforcing the exact expected length.
    /// The partial file is removed on any failure.
    pub async fn download_to(
        &self,
        path: &str,
        target: &Path,
        expected: u64,
    ) -> Result<(), NetError> {
        let response = self.execute(self.transfer.get(self.url(path))).await?;
        if response
            .content_length()
            .is_some_and(|length| length != expected)
        {
            return Err(NetError::Invalid);
        }
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut output = options.open(target).await.map_err(|_| NetError::Local)?;
        let result = async {
            let mut stream = response.bytes_stream();
            let mut total = 0u64;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| NetError::Offline)?;
                total = total
                    .checked_add(chunk.len() as u64)
                    .ok_or(NetError::TooLarge)?;
                if total > expected {
                    return Err(NetError::TooLarge);
                }
                output
                    .write_all(&chunk)
                    .await
                    .map_err(|_| NetError::Local)?;
            }
            // A short body is a dropped connection: transient, retried next round.
            if total != expected {
                return Err(NetError::Offline);
            }
            output.flush().await.map_err(|_| NetError::Local)?;
            output.sync_all().await.map_err(|_| NetError::Local)
        }
        .await;
        drop(output);
        if result.is_err() {
            let _ = tokio::fs::remove_file(target).await;
        }
        result
    }

    /// Uploads an already re-encoded public image (bounded by the caller).
    pub async fn post_public_copy<T: DeserializeOwned>(
        &self,
        path: &str,
        safe_name: &str,
        bytes: Vec<u8>,
    ) -> Result<T, NetError> {
        let request = self
            .transfer
            .post(self.url(path))
            .header("x-file-name", safe_name)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes);
        self.json(request, MAX_JSON_BYTES).await
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Minimal one-shot HTTP/1.1 server returning `head` then `body`.
    pub async fn serve_once(head: String, body: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        });
        format!("http://{address}")
    }

    fn chunked(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in body.chunks(1000) {
            out.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            out.extend_from_slice(chunk);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"0\r\n\r\n");
        out
    }

    #[tokio::test]
    async fn responses_are_bounded_without_trusting_content_length() {
        // Declared length above the cap: rejected before reading.
        let origin = serve_once(
            "HTTP/1.1 200 OK\r\ncontent-length: 5000\r\nconnection: close\r\n\r\n".into(),
            vec![b'a'; 5000],
        )
        .await;
        let api = Api::new(&origin, "t").unwrap();
        assert_eq!(
            read_bounded(api.execute(api.http.get(api.url("/"))).await.unwrap(), 4096).await,
            Err(NetError::TooLarge)
        );
        // Chunked body without Content-Length that exceeds the cap mid-stream.
        let origin = serve_once(
            "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n".into(),
            chunked(&vec![b'b'; 9000]),
        )
        .await;
        let api = Api::new(&origin, "t").unwrap();
        assert_eq!(
            read_bounded(api.execute(api.http.get(api.url("/"))).await.unwrap(), 4096).await,
            Err(NetError::TooLarge)
        );
        // Within the cap.
        let origin = serve_once(
            "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n".into(),
            chunked(b"{\"ok\":true}"),
        )
        .await;
        let api = Api::new(&origin, "t").unwrap();
        let value: serde_json::Value = api.get_json("/", 4096).await.unwrap();
        assert_eq!(value["ok"], true);
    }

    #[tokio::test]
    async fn redirects_are_not_followed_and_status_codes_are_typed() {
        let origin = serve_once("HTTP/1.1 302 Found\r\nlocation: https://elsewhere.test/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(), vec![]).await;
        let api = Api::new(&origin, "t").unwrap();
        assert!(matches!(
            api.get_json::<serde_json::Value>("/v1/vault", 4096).await,
            Err(NetError::Status { status: 302, .. })
        ));
        let origin = serve_once(
            "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(),
            vec![],
        )
        .await;
        let api = Api::new(&origin, "t").unwrap();
        assert_eq!(
            api.get_json::<serde_json::Value>("/v1/vault", 4096)
                .await
                .unwrap_err(),
            NetError::Revoked
        );
        let body = b"{\"code\":\"resync_required\",\"reason\":\"cursor_expired\"}".to_vec();
        let origin = serve_once(
            format!(
                "HTTP/1.1 409 Conflict\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            ),
            body,
        )
        .await;
        let api = Api::new(&origin, "t").unwrap();
        assert!(api
            .get_json::<serde_json::Value>("/v1/events", 4096)
            .await
            .unwrap_err()
            .is_status(409, "resync_required"));
        let body = b"{\"code\":\"<script>\"}".to_vec();
        let origin = serve_once(
            format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            ),
            body,
        )
        .await;
        let api = Api::new(&origin, "t").unwrap();
        assert_eq!(
            api.get_json::<serde_json::Value>("/", 4096)
                .await
                .unwrap_err(),
            NetError::Status {
                status: 400,
                code: None
            }
        );
    }

    #[tokio::test]
    async fn downloads_enforce_exact_length_and_remove_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let origin = serve_once(
            "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n".into(),
            chunked(&vec![7u8; 3000]),
        )
        .await;
        let api = Api::new(&origin, "t").unwrap();
        let target = dir.path().join("too-long");
        assert_eq!(
            api.download_to("/x", &target, 2000).await,
            Err(NetError::TooLarge)
        );
        assert!(!target.exists());
        let origin = serve_once(
            "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n".into(),
            chunked(&vec![7u8; 3000]),
        )
        .await;
        let api = Api::new(&origin, "t").unwrap();
        let target = dir.path().join("ok");
        api.download_to("/x", &target, 3000).await.unwrap();
        assert_eq!(std::fs::read(&target).unwrap().len(), 3000);
    }
}
