use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to read PEM file: {0}")]
    PemRead(#[from] std::io::Error),

    #[error("invalid RSA private key: {0}")]
    InvalidKey(#[from] jsonwebtoken::errors::Error),

    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("GitHub API error: {status} {body}")]
    Api { status: u16, body: String },

    #[error("no installation found for this app")]
    NoInstallation,
}

#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    expires_at: u64,
}

impl CachedToken {
    /// Returns true if the token expires within the next 5 minutes.
    fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.expires_at <= now + 300
    }
}

impl std::fmt::Debug for GitHubApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubApp")
            .field("app_id", &self.app_id)
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

pub struct GitHubApp {
    app_id: u64,
    private_key: jsonwebtoken::EncodingKey,
    client: Client,
    token: Mutex<Option<CachedToken>>,
    installation_id: Mutex<Option<u64>>,
    api_base: String,
}

#[derive(Debug, Deserialize)]
struct Installation {
    id: u64,
}

#[derive(Debug, Deserialize)]
struct AccessToken {
    token: String,
    expires_at: String,
}

impl GitHubApp {
    /// Create a `GitHubApp` by reading a PEM-encoded private key from disk.
    pub fn from_pem_file(app_id: u64, pem_path: &Path) -> Result<Self, Error> {
        let pem_bytes = std::fs::read(pem_path)?;
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(&pem_bytes)?;
        Ok(Self {
            app_id,
            private_key: key,
            client: Client::new(),
            token: Mutex::new(None),
            installation_id: Mutex::new(None),
            api_base: "https://api.github.com".to_string(),
        })
    }

    /// Create a `GitHubApp` directly from PEM bytes (useful for testing).
    pub fn from_pem_bytes(app_id: u64, pem_bytes: &[u8]) -> Result<Self, Error> {
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem_bytes)?;
        Ok(Self {
            app_id,
            private_key: key,
            client: Client::new(),
            token: Mutex::new(None),
            installation_id: Mutex::new(None),
            api_base: "https://api.github.com".to_string(),
        })
    }

    /// Generate a JWT for authenticating as the GitHub App.
    fn generate_jwt(&self) -> Result<String, Error> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let claims = serde_json::json!({
            "iss": self.app_id,
            "iat": now - 60,
            "exp": now + 600,
        });

        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        let token = jsonwebtoken::encode(&header, &claims, &self.private_key)?;
        Ok(token)
    }

    /// Discover the first installation ID for this app.
    async fn fetch_installation_id(&self) -> Result<u64, Error> {
        // Check cache first.
        if let Some(id) = *self.installation_id.lock().unwrap() {
            return Ok(id);
        }

        let jwt = self.generate_jwt()?;
        let url = format!("{}/app/installations", self.api_base);

        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {jwt}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "gauntlet-ci")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Api { status, body });
        }

        let installations: Vec<Installation> = resp.json().await?;
        let id = installations.first().ok_or(Error::NoInstallation)?.id;

        *self.installation_id.lock().unwrap() = Some(id);
        Ok(id)
    }

    /// Exchange the JWT for an installation access token.
    async fn fetch_access_token(&self, installation_id: u64) -> Result<CachedToken, Error> {
        let jwt = self.generate_jwt()?;
        let url = format!(
            "{}/app/installations/{installation_id}/access_tokens",
            self.api_base
        );

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {jwt}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "gauntlet-ci")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Api { status, body });
        }

        let access: AccessToken = resp.json().await?;

        // Parse the ISO 8601 expiry into a unix timestamp.
        let expires_at = parse_iso8601(&access.expires_at).unwrap_or_else(|| {
            // Fallback: assume 1 hour from now (GitHub's default).
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 3600
        });

        Ok(CachedToken {
            token: access.token,
            expires_at,
        })
    }

    /// Return a valid installation access token, refreshing if needed.
    pub async fn token(&self) -> Result<String, Error> {
        // Check for a cached, non-expired token.
        {
            let guard = self.token.lock().unwrap();
            if let Some(ref cached) = *guard
                && !cached.is_expired()
            {
                return Ok(cached.token.clone());
            }
        }

        // Fetch a fresh token.
        let installation_id = self.fetch_installation_id().await?;
        let cached = self.fetch_access_token(installation_id).await?;
        let token_str = cached.token.clone();

        *self.token.lock().unwrap() = Some(cached);
        Ok(token_str)
    }

    /// Override the API base URL (for testing).
    #[cfg(test)]
    fn set_api_base(&mut self, base: String) {
        self.api_base = base;
    }
}

/// Minimal ISO 8601 parser for GitHub's `expires_at` format (e.g. "2024-01-01T00:00:00Z").
fn parse_iso8601(s: &str) -> Option<u64> {
    // GitHub returns: "2024-11-22T14:30:00Z"
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let year: u64 = parts[0].parse().ok()?;
    let month: u64 = parts[1].parse().ok()?;
    let day: u64 = parts[2].parse().ok()?;

    let time_parts: Vec<&str> = time.split(':').collect();
    if time_parts.len() != 3 {
        return None;
    }
    let hour: u64 = time_parts[0].parse().ok()?;
    let min: u64 = time_parts[1].parse().ok()?;
    let sec: u64 = time_parts[2].parse().ok()?;

    // Days in each month (non-leap year base).
    let days_in_month = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);

    // Days from year 1970 to this year.
    let mut days: u64 = 0;
    for y in 1970..year {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        days += if leap { 366 } else { 365 };
    }

    // Days from months.
    for m in 1..month {
        days += days_in_month[m as usize];
        if m == 2 && is_leap {
            days += 1;
        }
    }

    days += day - 1;

    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{Algorithm, DecodingKey, Validation};

    /// Generate a test RSA key pair in PEM format.
    fn generate_test_pem() -> Vec<u8> {
        use std::process::Command;
        let output = Command::new("openssl")
            .args(["genrsa", "2048"])
            .output()
            .expect("openssl must be available for tests");
        assert!(output.status.success(), "openssl genrsa failed");
        output.stdout
    }

    #[test]
    fn test_jwt_generation_has_correct_claims() {
        let pem = generate_test_pem();
        let app = GitHubApp::from_pem_bytes(12345, &pem).unwrap();
        let jwt = app.generate_jwt().unwrap();

        // Decode the JWT header to verify algorithm.
        let header = jsonwebtoken::decode_header(&jwt).unwrap();
        assert_eq!(header.alg, Algorithm::RS256);

        // Decode payload without verification (we already verified the header).
        // JWT is base64url-encoded: header.payload.signature
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        use base64::Engine;
        let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&payload_bytes).unwrap();
        assert_eq!(claims["iss"], serde_json::json!(12345));

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let iat = claims["iat"].as_u64().unwrap();
        let exp = claims["exp"].as_u64().unwrap();

        // iat should be ~60 seconds before now.
        assert!(iat <= now && iat >= now - 120, "iat out of expected range");

        // exp should be ~10 minutes from now.
        assert!(exp > now && exp <= now + 660, "exp out of expected range");

        // exp - iat should be ~660 seconds (10min + 60s backdate).
        assert_eq!(exp - iat, 660);
    }

    #[test]
    fn test_from_pem_file() {
        let pem = generate_test_pem();
        let dir = tempfile::tempdir().unwrap();
        let pem_path = dir.path().join("test.pem");
        std::fs::write(&pem_path, &pem).unwrap();

        let app = GitHubApp::from_pem_file(42, &pem_path);
        assert!(app.is_ok());
    }

    #[test]
    fn test_from_pem_file_not_found() {
        let result = GitHubApp::from_pem_file(42, Path::new("/nonexistent/test.pem"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::PemRead(_)));
    }

    #[test]
    fn test_from_pem_bytes_invalid() {
        let result = GitHubApp::from_pem_bytes(42, b"not a real pem");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::InvalidKey(_)));
    }

    #[test]
    fn test_cached_token_expiry() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Token that expires in 10 minutes — not expired.
        let fresh = CachedToken {
            token: "ghs_fresh".into(),
            expires_at: now + 600,
        };
        assert!(!fresh.is_expired());

        // Token that expires in 4 minutes — expired (within 5min buffer).
        let stale = CachedToken {
            token: "ghs_stale".into(),
            expires_at: now + 240,
        };
        assert!(stale.is_expired());

        // Token that already expired.
        let dead = CachedToken {
            token: "ghs_dead".into(),
            expires_at: now - 60,
        };
        assert!(dead.is_expired());
    }

    #[test]
    fn test_token_returns_cached_when_valid() {
        let pem = generate_test_pem();
        let app = GitHubApp::from_pem_bytes(1, &pem).unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Pre-fill a cached token.
        *app.token.lock().unwrap() = Some(CachedToken {
            token: "ghs_cached_value".into(),
            expires_at: now + 3600,
        });

        // token() should return the cached value without making any HTTP calls.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let result = rt.block_on(app.token());
        assert_eq!(result.unwrap(), "ghs_cached_value");
    }

    #[test]
    fn test_parse_iso8601() {
        // Known value: 2024-01-01T00:00:00Z = 1704067200
        let ts = parse_iso8601("2024-01-01T00:00:00Z").unwrap();
        assert_eq!(ts, 1704067200);

        // 1970-01-01T00:00:00Z = 0
        let ts = parse_iso8601("1970-01-01T00:00:00Z").unwrap();
        assert_eq!(ts, 0);

        // Invalid strings.
        assert!(parse_iso8601("not-a-date").is_none());
        assert!(parse_iso8601("").is_none());
    }
}
