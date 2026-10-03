use std::{
    io::{Error as IoError, SeekFrom},
    path::PathBuf,
    pin::Pin,
    time::{Duration, SystemTime},
};

use aws_credential_types::Credentials;
use aws_sigv4::{
    http_request::{
        PayloadChecksumKind, PercentEncodingMode, SignableBody, SignableRequest, SigningParams,
        SigningSettings, UriPathNormalizationMode, sign,
    },
    sign::v4,
};
use futures_util::{Stream, StreamExt};
use reqwest::{
    Client, Method, RequestBuilder, StatusCode,
    header::{CONTENT_LENGTH, HeaderName, HeaderValue},
    redirect::Policy,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use url::Url;

use crate::config::S3Config;

pub type DataStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, IoError>> + Send + Sync + 'static>>;

/// TCP connect bound for every S3 request.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum silence while reading any S3 response, including streamed bodies.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Whole-request bound for small control requests (HEAD, DELETE, probes).
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
/// Whole-request bound for an object upload (attachments are at most 64 MiB).
const PUT_TIMEOUT: Duration = Duration::from_secs(300);

/// Storage failures callers must distinguish: a missing object is a durable
/// fact (409/404/410 at the API), anything else is transient (503).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum StorageError {
    #[error("storage object not found")]
    NotFound,
    #[error("storage unavailable: {0}")]
    Unavailable(&'static str),
    #[error("invalid storage request: {0}")]
    Invalid(&'static str),
}

impl StorageError {
    fn from_status(status: StatusCode, operation: &'static str) -> Self {
        if status == StatusCode::NOT_FOUND {
            Self::NotFound
        } else {
            Self::Unavailable(operation)
        }
    }
}

#[derive(Clone)]
pub struct Storage {
    endpoint: Url,
    bucket: String,
    credentials: Credentials,
    client: Client,
}

impl Storage {
    /// Constructs storage and idempotently provisions the configured private bucket.
    /// Normal API startup uses this so a fresh Compose deployment is usable without
    /// requiring a separate operator-only storage check first.
    pub async fn initialize(config: &S3Config) -> Result<Self, StorageError> {
        let storage = Self::new(config);
        storage.ensure_bucket().await?;
        Ok(storage)
    }

    pub fn new(config: &S3Config) -> Self {
        let credentials = Credentials::new(
            config.access_key.clone(),
            config.secret_key.clone(),
            None,
            None,
            "peppy-static-s3-config",
        );
        let client = Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .expect("static S3 HTTP client configuration is valid");
        Self {
            endpoint: config.endpoint.clone(),
            bucket: config.bucket.clone(),
            credentials,
            client,
        }
    }

    /// Streams a spooled file to `key` with a signed content hash and length.
    pub async fn put_file(&self, key: &str, path: PathBuf) -> Result<(), StorageError> {
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|_| StorageError::Invalid("unable to open S3 upload file"))?;
        let length = file
            .metadata()
            .await
            .map_err(|_| StorageError::Invalid("unable to inspect S3 upload file"))?
            .len();
        let mut digest = Sha256::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|_| StorageError::Invalid("unable to hash S3 upload file"))?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        file.seek(SeekFrom::Start(0))
            .await
            .map_err(|_| StorageError::Invalid("unable to rewind S3 upload file"))?;

        let url = self.object_url(key)?;
        let length_header = length.to_string();
        let body_hash = hex::encode(digest.finalize());
        let request = self.signed_request(
            Method::PUT,
            &url,
            &[("content-length", length_header.as_str())],
            SignableBody::Precomputed(body_hash),
        )?;
        let body = reqwest::Body::wrap_stream(ReaderStream::new(file));
        let response = request
            .header(CONTENT_LENGTH, length)
            .body(body)
            .timeout(PUT_TIMEOUT)
            .send()
            .await
            .map_err(|_| StorageError::Unavailable("S3 PUT request failed"))?;
        if !response.status().is_success() {
            return Err(StorageError::Unavailable("S3 PUT rejected"));
        }
        Ok(())
    }

    /// Uploads a small in-memory object (readiness contract checks only).
    pub async fn put_bytes(&self, key: &str, bytes: Vec<u8>) -> Result<(), StorageError> {
        let url = self.object_url(key)?;
        let request = self.signed_request(Method::PUT, &url, &[], SignableBody::Bytes(&bytes))?;
        let response = request
            .body(bytes)
            .timeout(CONTROL_TIMEOUT)
            .send()
            .await
            .map_err(|_| StorageError::Unavailable("S3 PUT request failed"))?;
        if !response.status().is_success() {
            return Err(StorageError::Unavailable("S3 PUT rejected"));
        }
        Ok(())
    }

    pub async fn head_bytes(&self, key: &str) -> Result<i64, StorageError> {
        let url = self.object_url(key)?;
        let response = self
            .signed_request(Method::HEAD, &url, &[], SignableBody::empty())?
            .timeout(CONTROL_TIMEOUT)
            .send()
            .await
            .map_err(|_| StorageError::Unavailable("S3 HEAD request failed"))?;
        if !response.status().is_success() {
            return Err(StorageError::from_status(
                response.status(),
                "S3 HEAD rejected",
            ));
        }
        response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|length| *length >= 0)
            .ok_or(StorageError::Unavailable(
                "S3 object has invalid content length",
            ))
    }

    /// Idempotent delete: an already absent object is success.
    pub async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let url = self.object_url(key)?;
        let response = self
            .signed_request(Method::DELETE, &url, &[], SignableBody::empty())?
            .timeout(CONTROL_TIMEOUT)
            .send()
            .await
            .map_err(|_| StorageError::Unavailable("S3 DELETE request failed"))?;
        if response.status().is_success() || response.status() == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(StorageError::Unavailable("S3 DELETE rejected"))
        }
    }

    /// Streams an object. Body stalls are bounded by the client read timeout.
    pub async fn get(&self, key: &str) -> Result<DataStream, StorageError> {
        let url = self.object_url(key)?;
        let response = self
            .signed_request(Method::GET, &url, &[], SignableBody::empty())?
            .send()
            .await
            .map_err(|_| StorageError::Unavailable("S3 GET request failed"))?;
        if !response.status().is_success() {
            return Err(StorageError::from_status(
                response.status(),
                "S3 GET rejected",
            ));
        }
        let stream = response.bytes_stream().map(|chunk| {
            chunk
                .map(|bytes| bytes.to_vec())
                .map_err(|_| IoError::other("S3 response stream failed"))
        });
        Ok(Box::pin(stream))
    }

    /// Signed service-level request used by readiness.
    pub async fn probe(&self) -> Result<(), StorageError> {
        let mut url = self.endpoint.clone();
        url.set_path("/");
        let response = self
            .signed_request(Method::GET, &url, &[], SignableBody::empty())?
            .timeout(CONTROL_TIMEOUT)
            .send()
            .await
            .map_err(|_| StorageError::Unavailable("S3 probe request failed"))?;
        if !response.status().is_success() {
            return Err(StorageError::Unavailable("S3 probe rejected"));
        }
        Ok(())
    }

    /// Creates the private bucket if needed; an existing bucket is success.
    pub async fn ensure_bucket(&self) -> Result<(), StorageError> {
        let url = self.bucket_url()?;
        let response = self
            .signed_request(Method::PUT, &url, &[], SignableBody::empty())?
            .body(Vec::new())
            .timeout(CONTROL_TIMEOUT)
            .send()
            .await
            .map_err(|_| StorageError::Unavailable("S3 bucket request failed"))?;
        if response.status().is_success() || response.status() == StatusCode::CONFLICT {
            Ok(())
        } else {
            Err(StorageError::Unavailable("S3 bucket provisioning rejected"))
        }
    }

    /// Status of an unsigned GET, used to prove the bucket is not public.
    pub async fn unsigned_get_status(&self, key: &str) -> Result<StatusCode, StorageError> {
        let url = self.object_url(key)?;
        self.client
            .get(url)
            .timeout(CONTROL_TIMEOUT)
            .send()
            .await
            .map(|response| response.status())
            .map_err(|_| StorageError::Unavailable("unsigned S3 GET failed"))
    }

    fn bucket_url(&self) -> Result<Url, StorageError> {
        let mut url = self.endpoint.clone();
        url.path_segments_mut()
            .map_err(|_| StorageError::Invalid("invalid S3 endpoint"))?
            .pop_if_empty()
            .push(&self.bucket);
        Ok(url)
    }

    fn object_url(&self, key: &str) -> Result<Url, StorageError> {
        if key.is_empty()
            || key
                .split('/')
                .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
        {
            return Err(StorageError::Invalid("invalid S3 object key"));
        }
        let mut url = self.endpoint.clone();
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| StorageError::Invalid("invalid S3 endpoint"))?;
        segments.pop_if_empty().push(&self.bucket);
        for segment in key.split('/') {
            segments.push(segment);
        }
        drop(segments);
        Ok(url)
    }

    fn signed_request(
        &self,
        method: Method,
        url: &Url,
        headers: &[(&str, &str)],
        body: SignableBody<'_>,
    ) -> Result<RequestBuilder, StorageError> {
        let identity = self.credentials.clone().into();
        let mut settings = SigningSettings::default();
        // S3 canonicalizes the already encoded object path without path normalization.
        settings.percent_encoding_mode = PercentEncodingMode::Single;
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region("us-east-1")
            .name("s3")
            .time(SystemTime::now())
            .settings(settings)
            .build()
            .map_err(|_| StorageError::Invalid("unable to construct S3 signing parameters"))?;
        let params: SigningParams<'_> = params.into();
        let signable =
            SignableRequest::new(method.as_str(), url.as_str(), headers.iter().copied(), body)
                .map_err(|_| StorageError::Invalid("unable to construct S3 signing request"))?;
        let (instructions, _) = sign(signable, &params)
            .map_err(|_| StorageError::Invalid("unable to sign S3 request"))?
            .into_parts();

        let mut request = self.client.request(method, url.clone());
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| StorageError::Invalid("invalid S3 request header"))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| StorageError::Invalid("invalid S3 request header"))?;
            request = request.header(name, value);
        }
        for (name, value) in instructions.headers() {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| StorageError::Invalid("invalid signed S3 header"))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| StorageError::Invalid("invalid signed S3 header"))?;
            request = request.header(name, value);
        }
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_not_found_is_a_durable_absence() {
        assert_eq!(
            StorageError::from_status(StatusCode::NOT_FOUND, "x"),
            StorageError::NotFound
        );
        for status in [
            StatusCode::FORBIDDEN,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert_eq!(
                StorageError::from_status(status, "x"),
                StorageError::Unavailable("x")
            );
        }
    }
}
