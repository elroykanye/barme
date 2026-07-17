//! Server configuration, loaded from `barme.toml` with environment overrides.
//!
//! Precedence: built-in defaults < `barme.toml` < environment. A missing file
//! is fine (defaults apply); a malformed one is an error worth surfacing.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub data_dir: String,
    pub s3_addr: String,
    pub native_addr: String,
    pub cdn_addr: String,
    pub console_addr: String,
    /// Allowed CORS origins for the browser-facing doors. `["*"]` means any.
    pub cors_origins: Vec<String>,
    /// How long a chunk must sit condemned before GC erases it.
    pub gc_grace_secs: u64,
    /// How often the GC sweep runs.
    pub gc_interval_secs: u64,
    pub default_policy: PolicyConfig,
    /// Largest upload accepted on the S3 and native doors, in bytes. Uploads are
    /// buffered in memory, so this caps per-request memory and stops a large
    /// upload from OOM-killing the process. Rejected with `413 Payload Too
    /// Large`. Size it below the memory available to the server.
    pub max_upload_bytes: usize,
    /// Bootstrap owner credential. Multi-key management lives on top of this.
    pub credentials: Option<Credential>,
    pub embed_url: Option<String>,
    pub embed_model: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    pub codec: String,
    pub zstd_level: i32,
    pub tenant: String,
    pub policy_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Credential {
    pub access_key: String,
    pub secret_key: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            data_dir: "./barme-data".into(),
            s3_addr: "0.0.0.0:9000".into(),
            native_addr: "0.0.0.0:7373".into(),
            cdn_addr: "0.0.0.0:7375".into(),
            console_addr: "0.0.0.0:7374".into(),
            cors_origins: vec!["*".into()],
            gc_grace_secs: 86_400,
            gc_interval_secs: 3_600,
            default_policy: PolicyConfig::default(),
            // 512 MiB. Generous for real objects, but bounded so a huge upload
            // can't exhaust memory. Raise it in barme.toml if your host has the
            // RAM and you push bigger blobs.
            max_upload_bytes: 512 * 1024 * 1024,
            // A known default so the door isn't open out of the box. Override in
            // barme.toml or via BARME_ACCESS_KEY / BARME_SECRET_KEY.
            credentials: Some(Credential {
                access_key: "barme".into(),
                secret_key: "barme".into(),
            }),
            embed_url: None,
            embed_model: String::new(),
        }
    }
}

impl Default for PolicyConfig {
    fn default() -> Self {
        PolicyConfig {
            codec: "zstd".into(),
            zstd_level: 0,
            tenant: "default".into(),
            policy_name: "default@v1".into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {0}: {1}")]
    Io(String, std::io::Error),
    #[error("parsing {0}: {1}")]
    Parse(String, toml::de::Error),
}

impl Config {
    /// Load config from `$BARME_CONFIG` (default `barme.toml`), then apply env
    /// overrides. Missing file -> defaults.
    pub fn load() -> Result<Config, ConfigError> {
        let path = std::env::var("BARME_CONFIG").unwrap_or_else(|_| "barme.toml".into());
        let mut cfg = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).map_err(|e| ConfigError::Parse(path.clone(), e))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(ConfigError::Io(path, e)),
        };
        cfg.apply_env();
        Ok(cfg)
    }

    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("BARME_DATA_DIR") {
            self.data_dir = v;
        }
        if let Ok(v) = std::env::var("BARME_EMBED_URL") {
            self.embed_url = Some(v);
        }
        if let Ok(v) = std::env::var("BARME_EMBED_MODEL") {
            self.embed_model = v;
        }
        if let Ok(v) = std::env::var("BARME_MAX_UPLOAD_BYTES") {
            if let Ok(n) = v.parse() {
                self.max_upload_bytes = n;
            }
        }
        if let (Ok(access), Ok(secret)) = (
            std::env::var("BARME_ACCESS_KEY"),
            std::env::var("BARME_SECRET_KEY"),
        ) {
            if !access.is_empty() && !secret.is_empty() {
                self.credentials = Some(Credential {
                    access_key: access,
                    secret_key: secret,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_partial_toml_over_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            data_dir = "/data/barme"
            gc_grace_secs = 60
            [default_policy]
            codec = "none"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.data_dir, "/data/barme");
        assert_eq!(cfg.gc_grace_secs, 60);
        assert_eq!(cfg.default_policy.codec, "none");
        // Untouched fields keep their defaults.
        assert_eq!(cfg.s3_addr, "0.0.0.0:9000");
        assert_eq!(cfg.default_policy.zstd_level, 0);
    }
}
