use std::{env, fmt, net::SocketAddr, time::Duration};

use ipnet::IpNet;
use thiserror::Error;
use url::Url;

pub const DEFAULT_REPLAY_RETENTION_DAYS: u32 = 30;
const MAX_REPLAY_RETENTION_DAYS: u32 = 3650;

#[derive(Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub database_url: String,
    pub s3: Option<S3Config>,
    pub public_api_url: Option<Url>,
    pub public_attachment_url: Option<Url>,
    pub vault_attachment_quota_bytes: i64,
    pub trusted_proxy_cidrs: Vec<IpNet>,
    /// Transport replay retention. Immutable snapshot records are retained separately.
    pub replay_retention: Duration,
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("bind_addr", &self.bind_addr)
            .field("database_url", &"[redacted]")
            .field("s3_configured", &self.s3.is_some())
            .field("public_api_url", &self.public_api_url)
            .field("public_attachment_url", &self.public_attachment_url)
            .field(
                "vault_attachment_quota_bytes",
                &self.vault_attachment_quota_bytes,
            )
            .field("trusted_proxy_cidrs", &self.trusted_proxy_cidrs)
            .field("replay_retention", &self.replay_retention)
            .finish()
    }
}

#[derive(Clone)]
pub struct S3Config {
    pub endpoint: Url,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub readiness_timeout: Duration,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{0} is required")]
    Missing(&'static str),
    #[error("invalid {name}: {message}")]
    Invalid { name: &'static str, message: String },
    #[error("BIND_ADDR must be loopback outside a container")]
    UnsafeBind,
    #[error("{name} must be an external HTTP(S) URL, not an internal service URL")]
    InternalPublicUrl { name: &'static str },
    #[error("{name} must use HTTPS when PEPPY_ENV=production")]
    InsecureProductionUrl { name: &'static str },
    #[error("S3_INTERNAL_ENDPOINT, S3_ACCESS_KEY, and S3_SECRET_KEY must be configured together")]
    IncompleteS3,
    #[error("S3_INTERNAL_ENDPOINT must be an origin without a path, query, or fragment")]
    S3EndpointMustBeOrigin,
    #[error("{name} contains characters unsafe for the SeaweedFS JSON configuration")]
    UnsafeS3Credential { name: &'static str },
    #[error("S3_BUCKET must be 3-63 lowercase DNS-compatible characters")]
    InvalidS3Bucket,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_get(|name| env::var(name).ok())
    }

    fn from_get(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let bind_addr = get("BIND_ADDR").unwrap_or_else(|| "127.0.0.1:8080".into());
        let bind_addr = bind_addr
            .parse::<SocketAddr>()
            .map_err(|source| ConfigError::Invalid {
                name: "BIND_ADDR",
                message: source.to_string(),
            })?;
        if !bind_addr.ip().is_loopback() && get("RUNNING_IN_CONTAINER").as_deref() != Some("1") {
            return Err(ConfigError::UnsafeBind);
        }

        let database_url = required(&get, "DATABASE_URL")?;
        let production = get("PEPPY_ENV").as_deref() == Some("production");
        let public_api_url = public_url(&get, "PUBLIC_API_URL", production)?;
        let public_attachment_url = public_url(&get, "PUBLIC_ATTACHMENT_URL", production)?;
        let vault_attachment_quota_bytes = get("VAULT_ATTACHMENT_QUOTA_BYTES")
            .map(|value| {
                value.parse::<i64>().map_err(|source| ConfigError::Invalid {
                    name: "VAULT_ATTACHMENT_QUOTA_BYTES",
                    message: source.to_string(),
                })
            })
            .transpose()?
            .unwrap_or(512 * 1024 * 1024);
        if vault_attachment_quota_bytes < 1 {
            return Err(ConfigError::Invalid {
                name: "VAULT_ATTACHMENT_QUOTA_BYTES",
                message: "must be positive".into(),
            });
        }
        let replay_retention_days = get("PEPPY_REPLAY_RETENTION_DAYS")
            .map(|value| {
                value.parse::<u32>().map_err(|source| ConfigError::Invalid {
                    name: "PEPPY_REPLAY_RETENTION_DAYS",
                    message: source.to_string(),
                })
            })
            .transpose()?
            .unwrap_or(DEFAULT_REPLAY_RETENTION_DAYS);
        if !(1..=MAX_REPLAY_RETENTION_DAYS).contains(&replay_retention_days) {
            return Err(ConfigError::Invalid {
                name: "PEPPY_REPLAY_RETENTION_DAYS",
                message: format!("must be between 1 and {MAX_REPLAY_RETENTION_DAYS}"),
            });
        }
        let replay_retention = Duration::from_secs(u64::from(replay_retention_days) * 86_400);
        let trusted_proxy_cidrs = get("TRUSTED_PROXY_CIDRS")
            .unwrap_or_default()
            .split(',')
            .filter(|value| !value.is_empty())
            .map(|value| {
                value
                    .parse()
                    .map_err(|source: ipnet::AddrParseError| ConfigError::Invalid {
                        name: "TRUSTED_PROXY_CIDRS",
                        message: source.to_string(),
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let endpoint = get("S3_INTERNAL_ENDPOINT");
        let access_key = get("S3_ACCESS_KEY");
        let secret_key = get("S3_SECRET_KEY");
        let bucket = get("S3_BUCKET");
        let s3 = match (endpoint, access_key, secret_key) {
            (None, None, None) => None,
            (Some(endpoint), Some(access_key), Some(secret_key))
                if !access_key.is_empty() && !secret_key.is_empty() =>
            {
                Some(S3Config {
                    endpoint: endpoint.parse().map_err(|source: url::ParseError| {
                        ConfigError::Invalid {
                            name: "S3_INTERNAL_ENDPOINT",
                            message: source.to_string(),
                        }
                    })?,
                    bucket: bucket.ok_or(ConfigError::Missing("S3_BUCKET"))?,
                    access_key,
                    secret_key,
                    readiness_timeout: Duration::from_secs(2),
                })
            }
            _ => return Err(ConfigError::IncompleteS3),
        };

        if let Some(s3) = &s3 {
            if s3.endpoint.path() != "/"
                || s3.endpoint.query().is_some()
                || s3.endpoint.fragment().is_some()
            {
                return Err(ConfigError::S3EndpointMustBeOrigin);
            }
            if !(3..=63).contains(&s3.bucket.len())
                || !s3
                    .bucket
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                || s3.bucket.starts_with('-')
                || s3.bucket.ends_with('-')
            {
                return Err(ConfigError::InvalidS3Bucket);
            }
            for (name, value) in [
                ("S3_ACCESS_KEY", &s3.access_key),
                ("S3_SECRET_KEY", &s3.secret_key),
            ] {
                if !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                {
                    return Err(ConfigError::UnsafeS3Credential { name });
                }
            }
        }

        Ok(Self {
            bind_addr,
            database_url,
            s3,
            public_api_url,
            public_attachment_url,
            vault_attachment_quota_bytes,
            trusted_proxy_cidrs,
            replay_retention,
        })
    }
}

fn required(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<String, ConfigError> {
    get(name)
        .filter(|value| !value.is_empty())
        .ok_or(ConfigError::Missing(name))
}

fn public_url(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    production: bool,
) -> Result<Option<Url>, ConfigError> {
    let Some(value) = get(name).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let url: Url = value
        .parse()
        .map_err(|source: url::ParseError| ConfigError::Invalid {
            name,
            message: source.to_string(),
        })?;
    let host = url.host_str().unwrap_or_default();
    if !matches!(url.scheme(), "http" | "https")
        || host.ends_with(".internal")
        || matches!(host, "api" | "migrate" | "postgres" | "seaweedfs")
    {
        return Err(ConfigError::InternalPublicUrl { name });
    }
    if production && url.scheme() != "https" {
        return Err(ConfigError::InsecureProductionUrl { name });
    }
    Ok(Some(url))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(name: &str) -> Option<String> {
        match name {
            "DATABASE_URL" => Some("postgres://peppy:secret@localhost/peppy".into()),
            _ => None,
        }
    }

    #[test]
    fn rejects_wildcard_development_bind() {
        let error = Config::from_get(|name| {
            if name == "BIND_ADDR" {
                Some("0.0.0.0:8080".into())
            } else {
                base(name)
            }
        })
        .unwrap_err();
        assert!(matches!(error, ConfigError::UnsafeBind));
    }

    #[test]
    fn rejects_internal_public_url() {
        let error = Config::from_get(|name| {
            if name == "PUBLIC_API_URL" {
                Some("http://seaweedfs:8333".into())
            } else {
                base(name)
            }
        })
        .unwrap_err();
        assert!(matches!(error, ConfigError::InternalPublicUrl { .. }));
    }

    #[test]
    fn rejects_incomplete_s3_credentials() {
        let error = Config::from_get(|name| {
            if name == "S3_INTERNAL_ENDPOINT" {
                Some("http://localhost:8333".into())
            } else {
                base(name)
            }
        })
        .unwrap_err();
        assert!(matches!(error, ConfigError::IncompleteS3));
    }

    #[test]
    fn rejects_plaintext_production_origin() {
        let error = Config::from_get(|name| match name {
            "PEPPY_ENV" => Some("production".into()),
            "PUBLIC_API_URL" => Some("http://example.test".into()),
            _ => base(name),
        })
        .unwrap_err();
        assert!(matches!(error, ConfigError::InsecureProductionUrl { .. }));
    }

    #[test]
    fn replay_retention_defaults_to_thirty_days_and_rejects_zero() {
        let config = Config::from_get(base).unwrap();
        assert_eq!(config.replay_retention, Duration::from_secs(30 * 86_400));
        let config = Config::from_get(|name| match name {
            "PEPPY_REPLAY_RETENTION_DAYS" => Some("7".into()),
            _ => base(name),
        })
        .unwrap();
        assert_eq!(config.replay_retention, Duration::from_secs(7 * 86_400));
        let error = Config::from_get(|name| match name {
            "PEPPY_REPLAY_RETENTION_DAYS" => Some("0".into()),
            _ => base(name),
        })
        .unwrap_err();
        assert!(matches!(
            error,
            ConfigError::Invalid {
                name: "PEPPY_REPLAY_RETENTION_DAYS",
                ..
            }
        ));
    }

    #[test]
    fn rejects_s3_endpoint_paths() {
        let error = Config::from_get(|name| match name {
            "S3_INTERNAL_ENDPOINT" => Some("http://localhost:8333/not-an-origin".into()),
            "S3_ACCESS_KEY" | "S3_SECRET_KEY" => Some("safe_key-1".into()),
            "S3_BUCKET" => Some("peppy-private".into()),
            _ => base(name),
        })
        .unwrap_err();
        assert!(matches!(error, ConfigError::S3EndpointMustBeOrigin));
    }
}
