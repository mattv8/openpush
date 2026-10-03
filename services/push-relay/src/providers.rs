//! Fixed-destination, content-free APNs and FCM HTTP v1 delivery adapters.

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{Provider, ProviderError, PushProvider};

const APNS_URL: &str = "https://api.push.apple.com/3/device/";
const APNS_SANDBOX_URL: &str = "https://api.sandbox.push.apple.com/3/device/";
const FCM_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const FCM_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct ConfiguredProvider {
    client: Client,
    apns: Option<ApnsConfig>,
    fcm: Option<FcmConfig>,
    apns_token: Arc<Mutex<Option<CachedToken>>>,
    fcm_token: Arc<Mutex<Option<CachedToken>>>,
}
#[derive(Clone)]
struct CachedToken {
    value: String,
    expires_at: SystemTime,
}

#[derive(Clone)]
struct ApnsConfig {
    team_id: String,
    key_id: String,
    bundle_id: String,
    key: Arc<String>,
    endpoint: &'static str,
}
#[derive(Clone)]
struct FcmConfig {
    project_id: String,
    client_email: String,
    private_key: Arc<String>,
}

#[derive(Deserialize)]
struct FcmSecret {
    project_id: String,
    client_email: String,
    private_key: String,
}

impl ConfiguredProvider {
    /// Empty provider selections are intentionally rejected: the safe default is unconfigured.
    pub fn from_environment(selection: &str) -> Result<Self, String> {
        let selected: Vec<_> = selection
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if selected.is_empty() || selected.iter().any(|p| *p != "apns" && *p != "fcm") {
            return Err("PEPPY_RELAY_PROVIDER must be unconfigured, apns, fcm, or apns,fcm".into());
        }
        let read = |name: &str| -> Result<String, String> {
            if let Ok(value) = std::env::var(name) {
                return Ok(value);
            }
            let path = std::env::var(format!("{name}_FILE"))
                .map_err(|_| format!("{name} or {name}_FILE is required"))?;
            std::fs::read_to_string(path).map_err(|_| format!("cannot read {name}_FILE"))
        };
        let apns: Option<ApnsConfig> = selected
            .contains(&"apns")
            .then(|| {
                let team_id = identifier(
                    read("PEPPY_RELAY_APNS_TEAM_ID")?,
                    "PEPPY_RELAY_APNS_TEAM_ID",
                )?;
                let key_id =
                    identifier(read("PEPPY_RELAY_APNS_KEY_ID")?, "PEPPY_RELAY_APNS_KEY_ID")?;
                let bundle_id = identifier(
                    read("PEPPY_RELAY_APNS_BUNDLE_ID")?,
                    "PEPPY_RELAY_APNS_BUNDLE_ID",
                )?;
                let key = read("PEPPY_RELAY_APNS_PRIVATE_KEY")?.trim().to_owned();
                EncodingKey::from_ec_pem(key.as_bytes()).map_err(|_| "invalid APNs private key")?;
                let endpoint = match std::env::var("PEPPY_RELAY_APNS_ENV").as_deref() {
                    Ok("sandbox") => APNS_SANDBOX_URL,
                    Ok("production") | Err(_) => APNS_URL,
                    _ => return Err("PEPPY_RELAY_APNS_ENV must be sandbox or production".into()),
                };
                Ok::<_, String>(ApnsConfig {
                    team_id,
                    key_id,
                    bundle_id,
                    key: Arc::new(key),
                    endpoint,
                })
            })
            .transpose()?;
        let fcm: Option<FcmConfig> = selected
            .contains(&"fcm")
            .then(|| {
                let secret: FcmSecret = serde_json::from_str(&read(
                    "PEPPY_RELAY_FCM_SERVICE_ACCOUNT",
                )?)
                .map_err(|_| "PEPPY_RELAY_FCM_SERVICE_ACCOUNT must be service-account JSON")?;
                let project_id = identifier(secret.project_id, "FCM project_id")?;
                let client_email = identifier(secret.client_email, "FCM client_email")?;
                let private_key = secret.private_key.trim().to_owned();
                EncodingKey::from_rsa_pem(private_key.as_bytes())
                    .map_err(|_| "invalid FCM private key")?;
                Ok::<_, String>(FcmConfig {
                    project_id,
                    client_email,
                    private_key: Arc::new(private_key),
                })
            })
            .transpose()?;
        let client = Client::builder()
            .timeout(PROVIDER_TIMEOUT)
            .build()
            .map_err(|_| "cannot initialize provider HTTP client")?;
        Ok(Self {
            client,
            apns,
            fcm,
            apns_token: Arc::new(Mutex::new(None)),
            fcm_token: Arc::new(Mutex::new(None)),
        })
    }

    async fn send(
        &self,
        provider: Provider,
        token: &str,
        kind: &str,
        value: &str,
    ) -> Result<(), ProviderError> {
        match provider {
            Provider::Apns => match &self.apns {
                Some(config) => self.apns_send(config, token, kind, value).await,
                None => Err(ProviderError::Unavailable),
            },
            Provider::Fcm => match &self.fcm {
                Some(config) => self.fcm_send(config, token, kind, value).await,
                None => Err(ProviderError::Unavailable),
            },
        }
    }
    async fn apns_send(
        &self,
        c: &ApnsConfig,
        token: &str,
        kind: &str,
        value: &str,
    ) -> Result<(), ProviderError> {
        if !token.bytes().all(|b| b.is_ascii_hexdigit()) || !(64..=256).contains(&token.len()) {
            return Err(ProviderError::InvalidToken);
        }
        let jwt = self.apns_jwt(c).await?;
        let response = self.client.post(format!("{}{}", c.endpoint, token)).header("authorization", format!("bearer {jwt}")).header("apns-topic", &c.bundle_id).header("apns-push-type", "background").header("apns-priority", "5").json(&serde_json::json!({"aps":{"content-available":1},"peppy":{"kind":kind,"value":value}})).send().await.map_err(|_| ProviderError::Retryable)?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let result = classify_apns(status, &body);
        if matches!(result, Err(ProviderError::Retryable))
            && (status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN)
        {
            *self.apns_token.lock().await = None;
        }
        result
    }
    async fn apns_jwt(&self, c: &ApnsConfig) -> Result<String, ProviderError> {
        let mut cache = self.apns_token.lock().await;
        if let Some(token) = cache
            .as_ref()
            .filter(|token| token.expires_at > SystemTime::now())
        {
            return Ok(token.value.clone());
        }
        #[derive(Serialize)]
        struct Claims<'a> {
            iss: &'a str,
            iat: u64,
        }
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(c.key_id.clone());
        let jwt = encode(
            &header,
            &Claims {
                iss: &c.team_id,
                iat: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            },
            &EncodingKey::from_ec_pem(c.key.as_bytes()).map_err(|_| ProviderError::Unavailable)?,
        )
        .map_err(|_| ProviderError::Unavailable)?;
        *cache = Some(CachedToken {
            value: jwt.clone(),
            expires_at: SystemTime::now() + Duration::from_secs(45 * 60),
        });
        Ok(jwt)
    }
    async fn fcm_send(
        &self,
        c: &FcmConfig,
        token: &str,
        kind: &str,
        value: &str,
    ) -> Result<(), ProviderError> {
        #[derive(Serialize)]
        struct Claims<'a> {
            iss: &'a str,
            scope: &'a str,
            aud: &'a str,
            iat: u64,
            exp: u64,
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let assertion = encode(
            &Header::new(Algorithm::RS256),
            &Claims {
                iss: &c.client_email,
                scope: FCM_SCOPE,
                aud: FCM_TOKEN_URL,
                iat: now,
                exp: now + 300,
            },
            &EncodingKey::from_rsa_pem(c.private_key.as_bytes())
                .map_err(|_| ProviderError::Unavailable)?,
        )
        .map_err(|_| ProviderError::Unavailable)?;
        if let Some(cached) = self
            .fcm_token
            .lock()
            .await
            .as_ref()
            .filter(|token| token.expires_at > SystemTime::now())
            .cloned()
        {
            return self
                .fcm_message(c, token.to_owned(), cached, kind, value)
                .await;
        }
        #[derive(Deserialize)]
        struct Token {
            access_token: String,
            expires_in: u64,
        }
        let access: Token = self
            .client
            .post(FCM_TOKEN_URL)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &assertion),
            ])
            .send()
            .await
            .map_err(|_| ProviderError::Retryable)?
            .json()
            .await
            .map_err(|_| ProviderError::Retryable)?;
        let cached = CachedToken {
            value: access.access_token,
            expires_at: SystemTime::now()
                + Duration::from_secs(access.expires_in.saturating_sub(60)),
        };
        *self.fcm_token.lock().await = Some(cached.clone());
        self.fcm_message(c, token.to_owned(), cached, kind, value)
            .await
    }
    async fn fcm_message(
        &self,
        c: &FcmConfig,
        token: String,
        access: CachedToken,
        kind: &str,
        value: &str,
    ) -> Result<(), ProviderError> {
        let response = self.client.post(format!("https://fcm.googleapis.com/v1/projects/{}/messages:send", c.project_id)).bearer_auth(access.value).json(&serde_json::json!({"message":{"token":token,"android":{"priority":"high"},"data":{"kind":kind,"value":value}}})).send().await.map_err(|_| ProviderError::Retryable)?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let result = classify_fcm(status, &body);
        if matches!(result, Err(ProviderError::Retryable))
            && (status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN)
        {
            *self.fcm_token.lock().await = None;
        }
        result
    }
}

fn identifier(value: String, name: &str) -> Result<String, String> {
    let value = value.trim().to_owned();
    if value.is_empty()
        || value.len() > 255
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@'))
    {
        Err(format!("invalid {name}"))
    } else {
        Ok(value)
    }
}
fn classify_apns(status: StatusCode, body: &str) -> Result<(), ProviderError> {
    if status.is_success() {
        Ok(())
    } else if body.contains("BadDeviceToken") || body.contains("Unregistered") {
        Err(ProviderError::InvalidToken)
    } else if status == StatusCode::UNAUTHORIZED
        || body.contains("ExpiredProviderToken")
        || status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
    {
        Err(ProviderError::Retryable)
    } else {
        Err(ProviderError::Permanent)
    }
}
fn classify_fcm(status: StatusCode, body: &str) -> Result<(), ProviderError> {
    if status.is_success() {
        Ok(())
    } else if body.contains("UNREGISTERED") {
        Err(ProviderError::InvalidToken)
    } else if status == StatusCode::UNAUTHORIZED
        || status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
    {
        Err(ProviderError::Retryable)
    } else {
        Err(ProviderError::Permanent)
    }
}
type ProviderFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ProviderError>> + Send + 'a>>;
impl PushProvider for ConfiguredProvider {
    fn configured(&self) -> bool {
        self.apns.is_some() || self.fcm.is_some()
    }
    fn supports(&self, provider: Provider) -> bool {
        match provider {
            Provider::Apns => self.apns.is_some(),
            Provider::Fcm => self.fcm.is_some(),
        }
    }
    fn provider_name(&self) -> &'static str {
        match (self.apns.is_some(), self.fcm.is_some()) {
            (true, true) => "apns,fcm",
            (true, false) => "apns",
            (false, true) => "fcm",
            _ => "unconfigured",
        }
    }
    fn send_challenge<'a>(&'a self, p: Provider, t: &'a str, c: &'a str) -> ProviderFuture<'a> {
        Box::pin(async move { self.send(p, t, "challenge", c).await })
    }
    fn send_wake<'a>(&'a self, p: Provider, t: &'a str, n: &'a str) -> ProviderFuture<'a> {
        Box::pin(async move { self.send(p, t, "wake", n).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_response_classification_is_precise() {
        assert_eq!(classify_apns(StatusCode::OK, ""), Ok(()));
        assert_eq!(
            classify_apns(StatusCode::BAD_REQUEST, r#"{"reason":"BadDeviceToken"}"#),
            Err(ProviderError::InvalidToken)
        );
        assert_eq!(
            classify_apns(
                StatusCode::BAD_REQUEST,
                r#"{"reason":"DeviceTokenNotForTopic"}"#
            ),
            Err(ProviderError::Permanent)
        );
        assert_eq!(
            classify_fcm(StatusCode::TOO_MANY_REQUESTS, ""),
            Err(ProviderError::Retryable)
        );
        assert_eq!(
            classify_fcm(
                StatusCode::NOT_FOUND,
                r#"{"error":{"status":"UNREGISTERED"}}"#
            ),
            Err(ProviderError::InvalidToken)
        );
    }
}
