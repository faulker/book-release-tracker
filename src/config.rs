//! Application configuration: struct definitions mirroring `config.toml`, loading with
//! environment-variable overrides for secrets, and validation. See `config.example.toml`
//! for the annotated reference; keep it in sync with the structs below.
//!
//! A few fields (interval_days, dashboard, notify.ntfy/email connection details)
//! aren't read yet; they're consumed by later milestones (notifiers, daemon,
//! dashboard) per PLAN.md.

use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

/// Environment variable that overrides `audiobookshelf.api_key`.
const ENV_ABS_API_KEY: &str = "BRT_ABS_API_KEY";
/// Environment variable that overrides `hardcover.token`.
const ENV_HARDCOVER_TOKEN: &str = "BRT_HARDCOVER_TOKEN";
/// Environment variable that overrides `notify.ntfy.token`.
const ENV_NTFY_TOKEN: &str = "BRT_NTFY_TOKEN";
/// Environment variable that overrides `notify.email.password`.
const ENV_SMTP_PASSWORD: &str = "BRT_SMTP_PASSWORD";

/// Top-level config, deserialized directly from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub audiobookshelf: AudiobookshelfConfig,
    pub hardcover: HardcoverConfig,
    pub audnexus: AudnexusConfig,
    pub check: CheckConfig,
    pub dashboard: DashboardConfig,
    pub notify: NotifyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudiobookshelfConfig {
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub library_ids: Vec<String>,
    /// Seconds the daemon waits at startup for ABS to answer `/ping` before
    /// giving up (useful when ABS runs in the same container but starts after
    /// us). 0 skips the wait entirely.
    #[serde(default = "default_startup_timeout_secs")]
    pub startup_timeout_secs: u64,
}

/// Default for `audiobookshelf.startup_timeout_secs` when absent from the config file.
fn default_startup_timeout_secs() -> u64 {
    300
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardcoverConfig {
    #[serde(default)]
    pub token: String,
    pub min_request_interval_ms: u64,
    pub max_retries: u32,
    /// ISO 639-1 code (e.g. "en"). When set, series books with editions only
    /// in other languages (translations listed as separate books) are
    /// filtered out. Empty = no filtering.
    #[serde(default)]
    pub language: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudnexusConfig {
    pub enabled: bool,
    pub region: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckConfig {
    /// How many days the daemon waits between scheduled checks. The dashboard
    /// offers human-friendly presets (daily, weekly, monthly). Defaulted so
    /// configs predating the days-based interval still load.
    #[serde(default = "default_interval_days")]
    pub interval_days: u64,
    pub db_path: String,
}

/// Default for `check.interval_days` when absent from the config file.
fn default_interval_days() -> u64 {
    7
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardConfig {
    pub enabled: bool,
    pub bind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyConfig {
    pub ntfy: NtfyConfig,
    pub email: EmailConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtfyConfig {
    pub enabled: bool,
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub topic: String,
    #[serde(default)]
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailConfig {
    pub enabled: bool,
    #[serde(default)]
    pub smtp_host: String,
    #[serde(default)]
    pub smtp_port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
}

/// Errors produced while loading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The config file could not be read from disk (e.g. does not exist).
    #[error("could not read config file at {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The config file was read but did not parse as valid TOML matching our schema.
    #[error("could not parse config file at {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    /// One or more fields failed validation; all problems are listed together.
    #[error("config validation failed:\n{}", .0.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n"))]
    Validation(Vec<String>),
    /// The config could not be serialized or written back to disk.
    #[error("could not write config file at {path}: {message}")]
    Write { path: String, message: String },
}

/// Loads config from `path`, applies environment-variable secret overrides, and
/// validates the result. Returns a `ConfigError::Validation` listing every problem
/// found, not just the first.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let mut config: Config = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source,
    })?;

    apply_env_overrides(&mut config, |name| std::env::var(name).ok());
    validate(&config)?;

    Ok(config)
}

/// Loads config from `path` without env overrides or validation: exactly what
/// the file says. Used by the dashboard settings page, which edits the file
/// itself and must not bake env-provided secrets into it.
pub fn load_file(path: &Path) -> Result<Config, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;
    toml::from_str(&raw).map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source,
    })
}

/// Validates what a config would look like at load time: env overrides applied
/// to a copy first, so an empty secret in the file passes when the env var
/// covers it. The input config is not modified.
pub fn validate_effective(config: &Config) -> Result<(), ConfigError> {
    let mut effective = config.clone();
    apply_env_overrides(&mut effective, |name| std::env::var(name).ok());
    validate(&effective)
}

/// Serializes `config` to TOML and writes it to `path` atomically (temp file
/// in the same directory, then rename), so a crash mid-write can't leave a
/// truncated config behind. Comments in the existing file are not preserved.
pub fn save(config: &Config, path: &Path) -> Result<(), ConfigError> {
    let write_err = |message: String| ConfigError::Write {
        path: path.display().to_string(),
        message,
    };

    let body = toml::to_string_pretty(config).map_err(|e| write_err(e.to_string()))?;
    let contents = format!(
        "# book-release-tracker config. Edited via the dashboard settings page;\n\
         # comments are not preserved on save. See config.example.toml for docs.\n\n{body}"
    );

    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &contents).map_err(|e| write_err(e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| write_err(e.to_string()))?;
    Ok(())
}

/// Overwrites secret fields from environment variables when set, taking precedence
/// over whatever was in the config file. Takes a `lookup` fn rather than reading
/// `std::env` directly so tests can inject values without mutating real process
/// env (which would race across parallel test threads).
fn apply_env_overrides(config: &mut Config, lookup: impl Fn(&str) -> Option<String>) {
    if let Some(v) = lookup(ENV_ABS_API_KEY) {
        config.audiobookshelf.api_key = v;
    }
    if let Some(v) = lookup(ENV_HARDCOVER_TOKEN) {
        config.hardcover.token = v;
    }
    if let Some(v) = lookup(ENV_NTFY_TOKEN) {
        config.notify.ntfy.token = v;
    }
    if let Some(v) = lookup(ENV_SMTP_PASSWORD) {
        config.notify.email.password = v;
    }
}

/// Validates a fully-loaded config, collecting every problem found rather than
/// failing on the first. Returns `Ok(())` if there are no problems.
fn validate(config: &Config) -> Result<(), ConfigError> {
    let mut problems = Vec::new();

    if reqwest::Url::parse(&config.audiobookshelf.base_url).is_err() {
        problems.push(format!(
            "audiobookshelf.base_url is not a valid URL: {:?}",
            config.audiobookshelf.base_url
        ));
    }
    if config.audiobookshelf.api_key.trim().is_empty() {
        problems.push(format!(
            "audiobookshelf.api_key is empty (set it in the config file or via {ENV_ABS_API_KEY})"
        ));
    }
    if config.hardcover.token.trim().is_empty() {
        problems.push(format!(
            "hardcover.token is empty (set it in the config file or via {ENV_HARDCOVER_TOKEN})"
        ));
    }

    if config.notify.ntfy.enabled {
        if config.notify.ntfy.server.trim().is_empty() {
            problems.push("notify.ntfy.server is required when notify.ntfy.enabled = true".into());
        }
        if config.notify.ntfy.topic.trim().is_empty() {
            problems.push("notify.ntfy.topic is required when notify.ntfy.enabled = true".into());
        }
    }

    if config.notify.email.enabled {
        if config.notify.email.smtp_host.trim().is_empty() {
            problems
                .push("notify.email.smtp_host is required when notify.email.enabled = true".into());
        }
        // Username/password are optional: many relays (localhost, internal
        // SMTP) accept unauthenticated mail. When a username is given the
        // password should be too, so credentials aren't half-set.
        if !config.notify.email.username.trim().is_empty()
            && config.notify.email.password.trim().is_empty()
        {
            problems.push(format!(
                "notify.email.password is empty but a username is set (set it in the config file or via {ENV_SMTP_PASSWORD})"
            ));
        }
        if config.notify.email.from.trim().is_empty() {
            problems.push("notify.email.from is required when notify.email.enabled = true".into());
        }
        if config.notify.email.to.trim().is_empty() {
            problems.push("notify.email.to is required when notify.email.enabled = true".into());
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::Validation(problems))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Writes `contents` to a temp file and returns the guard so callers can pass
    /// `.path()` to `load()`.
    fn write_temp_config(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("create temp file");
        file.write_all(contents.as_bytes())
            .expect("write temp file");
        file
    }

    #[test]
    fn parses_example_config() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
        // The example ships with empty secrets, so we expect validation to fail on
        // exactly the secret/notifier fields, but parsing itself must succeed.
        let raw = std::fs::read_to_string(&path).expect("read config.example.toml");
        let config: Config = toml::from_str(&raw).expect("parse config.example.toml");
        assert_eq!(config.audiobookshelf.base_url, "https://audiobookshelf");
        assert_eq!(config.check.interval_days, 7);
        assert_eq!(config.audiobookshelf.startup_timeout_secs, 300);
        assert_eq!(config.dashboard.bind, "127.0.0.1:8091");
        assert_eq!(config.hardcover.language, "en");
    }

    #[test]
    fn env_override_wins_over_file_value() {
        let mut config = Config {
            audiobookshelf: AudiobookshelfConfig {
                base_url: "https://example.com".into(),
                api_key: "from-file".into(),
                library_ids: vec![],
                startup_timeout_secs: 300,
            },
            hardcover: HardcoverConfig {
                token: "hc-token".into(),
                min_request_interval_ms: 1100,
                max_retries: 3,
                language: String::new(),
            },
            audnexus: AudnexusConfig {
                enabled: true,
                region: "us".into(),
            },
            check: CheckConfig {
                interval_days: 7,
                db_path: "test.db".into(),
            },
            dashboard: DashboardConfig {
                enabled: true,
                bind: "127.0.0.1:8091".into(),
            },
            notify: NotifyConfig {
                ntfy: NtfyConfig {
                    enabled: false,
                    server: String::new(),
                    topic: String::new(),
                    token: String::new(),
                },
                email: EmailConfig {
                    enabled: false,
                    smtp_host: String::new(),
                    smtp_port: 587,
                    username: String::new(),
                    password: String::new(),
                    from: String::new(),
                    to: String::new(),
                },
            },
        };

        apply_env_overrides(&mut config, |name| {
            (name == ENV_ABS_API_KEY).then(|| "from-env".to_string())
        });

        assert_eq!(config.audiobookshelf.api_key, "from-env");
    }

    #[test]
    fn validation_aggregates_multiple_errors() {
        let file = write_temp_config(
            r#"
            [audiobookshelf]
            base_url = "not a url"
            api_key = ""
            library_ids = []

            [hardcover]
            token = ""
            min_request_interval_ms = 1100
            max_retries = 3

            [audnexus]
            enabled = true
            region = "us"

            [check]
            interval_days = 7
            db_path = "test.db"

            [dashboard]
            enabled = true
            bind = "127.0.0.1:8091"

            [notify.ntfy]
            enabled = true

            [notify.email]
            enabled = false
            "#,
        );

        let err = load(file.path()).expect_err("expected validation failure");
        let ConfigError::Validation(problems) = err else {
            panic!("expected ConfigError::Validation, got {err:?}");
        };
        // base_url, api_key, hardcover.token, ntfy.server, ntfy.topic
        assert_eq!(problems.len(), 5);
    }

    /// Base config TOML with email enabled and the given username/password,
    /// host/from/to always valid, so validation only reacts to the credentials.
    fn email_config(username: &str, password: &str) -> tempfile::NamedTempFile {
        write_temp_config(&format!(
            r#"
            [audiobookshelf]
            base_url = "https://abs.example.com"
            api_key = "key"
            library_ids = []

            [hardcover]
            token = "tok"
            min_request_interval_ms = 1100
            max_retries = 3

            [audnexus]
            enabled = true
            region = "us"

            [check]
            interval_days = 7
            db_path = "test.db"

            [dashboard]
            enabled = true
            bind = "127.0.0.1:8091"

            [notify.ntfy]
            enabled = false

            [notify.email]
            enabled = true
            smtp_host = "smtp.example.com"
            smtp_port = 587
            username = "{username}"
            password = "{password}"
            from = "tracker@example.com"
            to = "me@example.com"
            "#
        ))
    }

    #[test]
    fn email_without_credentials_is_valid() {
        // Unauthenticated relays: blank username and password must pass.
        load(email_config("", "").path()).expect("blank credentials should validate");
    }

    #[test]
    fn email_username_without_password_is_rejected() {
        let err = load(email_config("user", "").path()).expect_err("expected validation failure");
        let ConfigError::Validation(problems) = err else {
            panic!("expected ConfigError::Validation, got {err:?}");
        };
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("password"), "{problems:?}");
    }

    #[test]
    fn save_and_load_file_roundtrip() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
        let raw = std::fs::read_to_string(&example).expect("read config.example.toml");
        let mut config: Config = toml::from_str(&raw).expect("parse config.example.toml");
        config.check.interval_days = 42;
        config.audiobookshelf.api_key = "secret".into();

        let file = write_temp_config("");
        save(&config, file.path()).expect("save config");

        let loaded = load_file(file.path()).expect("load_file");
        assert_eq!(loaded.check.interval_days, 42);
        assert_eq!(loaded.audiobookshelf.api_key, "secret");
        assert_eq!(loaded.dashboard.bind, "127.0.0.1:8091");

        let written = std::fs::read_to_string(file.path()).expect("read saved config");
        assert!(
            written.starts_with("# book-release-tracker config"),
            "{written}"
        );
    }

    #[test]
    fn missing_file_gives_clear_error() {
        let err = load(Path::new("/nonexistent/config.toml")).expect_err("expected read error");
        assert!(matches!(err, ConfigError::Read { .. }));
        assert!(err.to_string().contains("/nonexistent/config.toml"));
    }
}
