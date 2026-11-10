// SPDX-License-Identifier: MIT

use serde::Deserialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub use crate::models::{DEFAULT_GATEWAY_MODELS, MODEL_EFFORT_LEVELS, NON_CORE_KEYWORDS};

/// Default TCP port for the local gateway server and monitor (`6470` = `GYRO` in leetspeak).
pub const DEFAULT_PORT: u16 = 6470;

pub const ENV_GEMINI_API_KEY: &str = "GEMINI_API_KEY";
pub const ENV_API_KEY: &str = "AGY_GYRO_API_KEY";
pub const ENV_OAUTH: &str = "AGY_GYRO_OAUTH";
pub const ENV_HOST: &str = "AGY_GYRO_HOST";
pub const ENV_PORT: &str = "AGY_GYRO_PORT";
pub const ENV_AGY_PATH: &str = "AGY_GYRO_AGY_PATH";
pub const ENV_LOG_FILE: &str = "AGY_GYRO_LOG_FILE";
pub const ENV_MAX_RETRIES: &str = "AGY_GYRO_MAX_RETRIES";
pub const ENV_INITIAL_DELAY_MS: &str = "AGY_GYRO_INITIAL_DELAY_MS";
pub const ENV_MAX_DELAY_MS: &str = "AGY_GYRO_MAX_DELAY_MS";
pub const ENV_NO_JITTER: &str = "AGY_GYRO_NO_JITTER";
pub const ENV_NO_BUFFER: &str = "AGY_GYRO_NO_BUFFER";
pub const ENV_REQUEST_TIMEOUT_SECS: &str = "AGY_GYRO_REQUEST_TIMEOUT_SECS";

/// All configuration environment variables recognized by [`Cli::try_parse_with_config_from`].
pub const KNOWN_ENV_VARS: &[&str] = &[
    ENV_GEMINI_API_KEY,
    ENV_API_KEY,
    ENV_OAUTH,
    ENV_HOST,
    ENV_PORT,
    ENV_AGY_PATH,
    ENV_LOG_FILE,
    ENV_MAX_RETRIES,
    ENV_INITIAL_DELAY_MS,
    ENV_MAX_DELAY_MS,
    ENV_NO_JITTER,
    ENV_NO_BUFFER,
    ENV_REQUEST_TIMEOUT_SECS,
];

/// Converts a wildcard bind address (`0.0.0.0`) into a loopback connect address (`127.0.0.1`).
pub fn connect_host(bind_host: &str) -> &str {
    if bind_host == "0.0.0.0" {
        "127.0.0.1"
    } else {
        bind_host
    }
}

/// Returns the default configuration file path: `~/.gemini/config/agy-gyro.toml`.
pub fn default_config_path() -> Option<PathBuf> {
    std::env::home_dir().map(|home| home.join(".gemini").join("config").join("agy-gyro.toml"))
}

#[derive(Debug, Clone)]
pub struct Cli {
    pub config: Config,
    pub command: Commands,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Commands {
    /// Start the background agy-gyro daemon (if not already running)
    Server,
    /// Run agy CLI wrapped with the local proxy (default mode)
    Run(Vec<String>),
    /// Monitor live token usage and cost in a terminal UI dashboard
    Monitor,
    /// Stop the running background agy-gyro daemon
    Stop,
}

/// Configuration loaded from `~/.gemini/config/agy-gyro.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FileConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    #[serde(alias = "api-key")]
    pub api_key: Option<String>,
    pub oauth: Option<bool>,
    #[serde(alias = "agy-path")]
    pub agy_path: Option<String>,
    #[serde(alias = "log-file")]
    pub log_file: Option<PathBuf>,
    #[serde(alias = "max-retries")]
    pub max_retries: Option<u32>,
    #[serde(alias = "initial-delay-ms")]
    pub initial_delay_ms: Option<u64>,
    #[serde(alias = "max-delay-ms")]
    pub max_delay_ms: Option<u64>,
    #[serde(alias = "no-jitter")]
    pub no_jitter: Option<bool>,
    #[serde(alias = "no-buffer")]
    pub no_buffer: Option<bool>,
    #[serde(alias = "request-timeout-secs")]
    pub request_timeout_secs: Option<u64>,
}

impl FileConfig {
    /// Loads and parses a TOML configuration file if it exists at `path`.
    pub fn load_from_path(path: &Path) -> Result<Option<Self>, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => toml::from_str::<Self>(&contents)
                .map(Some)
                .map_err(|e| format!("Failed to parse {}: {}", path.display(), e)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(format!("Failed to read {}: {}", path.display(), err)),
        }
    }
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn env_parse<T: FromStr>(name: &str) -> Result<Option<T>, String>
where
    T::Err: std::fmt::Display,
{
    match env_non_empty(name) {
        Some(raw) => raw
            .trim()
            .parse::<T>()
            .map(Some)
            .map_err(|e| format!("Invalid value '{}' for {}: {}", raw, name, e)),
        None => Ok(None),
    }
}

fn env_bool(name: &str) -> Result<Option<bool>, String> {
    match env_non_empty(name) {
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Ok(Some(true)),
            "false" | "0" | "no" => Ok(Some(false)),
            _ => Err(format!("Invalid boolean value '{}' for {}", raw, name)),
        },
        None => Ok(None),
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Host address to bind or connect to the proxy server
    pub host: String,
    /// Port to listen on or connect to
    pub port: u16,
    /// Upstream Gemini API base URL
    pub upstream: String,
    /// Gemini API key for authenticating upstream requests
    pub api_key: String,
    /// Explicitly lock to OAuth passthrough mode (token and price tracking only)
    pub oauth: bool,
    /// Executable path or command name for Antigravity CLI
    pub agy_path: String,
    /// Optional log file path for agy-gyro proxy tracing logs
    pub log_file: Option<PathBuf>,
    /// Maximum retry attempts on retriable errors
    pub max_retries: u32,
    /// Initial retry backoff delay in milliseconds
    pub initial_delay_ms: u64,
    /// Maximum retry backoff delay in milliseconds
    pub max_delay_ms: u64,
    /// Disable jitter in exponential backoff calculation
    pub no_jitter: bool,
    /// Disable full stream chunk buffering (stream chunks immediately to client after first chunk)
    pub no_buffer: bool,
    /// Client request timeout in seconds (for long thinking / generation requests)
    pub request_timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: DEFAULT_PORT,
            upstream: "https://generativelanguage.googleapis.com".to_string(),
            api_key: String::new(),
            oauth: false,
            agy_path: "agy".to_string(),
            log_file: None,
            max_retries: 15,
            initial_delay_ms: 1000,
            max_delay_ms: 60000,
            no_jitter: false,
            no_buffer: false,
            request_timeout_secs: 600,
        }
    }
}

impl Config {
    /// Returns `true` if a non-empty Gemini API key is configured in this process.
    pub fn has_api_key(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    /// Returns `true` if Enterprise Gateway + retry/buffering proxy mode is enabled
    /// in this process (`!self.oauth && self.has_api_key()`).
    pub fn is_gateway_enabled(&self) -> bool {
        !self.oauth && self.has_api_key()
    }

    pub fn is_jitter_enabled(&self) -> bool {
        !self.no_jitter
    }

    pub fn is_buffer_enabled(&self) -> bool {
        !self.no_buffer
    }

    /// Returns the base HTTP URL (`http://<connect_host>:<port>`) for connecting to the local daemon.
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", connect_host(&self.host), self.port)
    }

    /// Returns the `/usage` SSE endpoint URL for connecting to the local daemon.
    pub fn usage_url(&self) -> String {
        format!("{}/usage", self.base_url())
    }

    /// Returns the `/logs` SSE endpoint URL for connecting to the local daemon.
    pub fn logs_url(&self) -> String {
        format!("{}/logs", self.base_url())
    }

    /// Serializes this [`Config`] into environment variables on a child [`std::process::Command`].
    pub fn apply_env_to_command(&self, cmd: &mut std::process::Command, port: u16) {
        cmd.env(ENV_HOST, &self.host)
            .env(ENV_PORT, port.to_string())
            .env(ENV_MAX_RETRIES, self.max_retries.to_string())
            .env(ENV_INITIAL_DELAY_MS, self.initial_delay_ms.to_string())
            .env(ENV_MAX_DELAY_MS, self.max_delay_ms.to_string())
            .env(ENV_NO_JITTER, self.no_jitter.to_string())
            .env(ENV_NO_BUFFER, self.no_buffer.to_string())
            .env(
                ENV_REQUEST_TIMEOUT_SECS,
                self.request_timeout_secs.to_string(),
            )
            .env_remove(ENV_OAUTH)
            .env(ENV_API_KEY, &self.api_key)
            .env(ENV_GEMINI_API_KEY, &self.api_key);

        if let Some(ref log_file) = self.log_file {
            cmd.env(ENV_LOG_FILE, log_file);
        }
        if !self.agy_path.is_empty() {
            cmd.env(ENV_AGY_PATH, &self.agy_path);
        }
    }
}

impl Cli {
    /// Loads action from the first CLI argument and settings from environment variables
    /// and `~/.gemini/config/agy-gyro.toml`, exiting on error.
    pub fn load() -> Self {
        Self::try_parse_with_config_from(std::env::args_os(), default_config_path().as_deref())
            .unwrap_or_else(|e| {
                eprintln!("error: {e}");
                std::process::exit(2);
            })
    }

    /// Parses the action from the first argument of `itr` (or passes arguments through to `agy`)
    /// and resolves settings from environment variables and an optional TOML configuration file.
    ///
    /// Settings precedence order (highest to lowest):
    /// 1. Environment variables
    /// 2. Configuration file (`~/.gemini/config/agy-gyro.toml`)
    /// 3. Built-in default values
    pub fn try_parse_with_config_from<I, T>(
        itr: I,
        config_path: Option<&Path>,
    ) -> Result<Self, String>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        let args: Vec<String> = itr
            .into_iter()
            .skip(1)
            .map(|a| a.into().to_string_lossy().into_owned())
            .collect();

        let file_config = match config_path {
            Some(path) => FileConfig::load_from_path(path)?.unwrap_or_default(),
            None => FileConfig::default(),
        };

        let host = env_non_empty(ENV_HOST)
            .or(file_config.host)
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let port = env_parse::<u16>(ENV_PORT)?
            .or(file_config.port)
            .unwrap_or(DEFAULT_PORT);
        let max_retries = env_parse::<u32>(ENV_MAX_RETRIES)?
            .or(file_config.max_retries)
            .unwrap_or(15);
        let initial_delay_ms = env_parse::<u64>(ENV_INITIAL_DELAY_MS)?
            .or(file_config.initial_delay_ms)
            .unwrap_or(1000);
        let max_delay_ms = env_parse::<u64>(ENV_MAX_DELAY_MS)?
            .or(file_config.max_delay_ms)
            .unwrap_or(60000);
        let no_jitter = env_bool(ENV_NO_JITTER)?
            .or(file_config.no_jitter)
            .unwrap_or(false);
        let no_buffer = env_bool(ENV_NO_BUFFER)?
            .or(file_config.no_buffer)
            .unwrap_or(false);
        let request_timeout_secs = env_parse::<u64>(ENV_REQUEST_TIMEOUT_SECS)?
            .or(file_config.request_timeout_secs)
            .unwrap_or(600);
        let api_key = env_non_empty(ENV_API_KEY)
            .or_else(|| env_non_empty(ENV_GEMINI_API_KEY))
            .or(file_config.api_key)
            .unwrap_or_default();
        let oauth = env_bool(ENV_OAUTH)?.or(file_config.oauth).unwrap_or(false);
        let agy_path = env_non_empty(ENV_AGY_PATH)
            .or(file_config.agy_path)
            .unwrap_or_else(|| "agy".to_string());
        let log_file = env_non_empty(ENV_LOG_FILE)
            .map(PathBuf::from)
            .or(file_config.log_file);

        let config = Config {
            host,
            port,
            upstream: "https://generativelanguage.googleapis.com".to_string(),
            max_retries,
            initial_delay_ms,
            max_delay_ms,
            no_jitter,
            no_buffer,
            request_timeout_secs,
            api_key,
            oauth,
            agy_path,
            log_file,
        };

        let command = match args.first().map(|s| s.as_str()) {
            Some("server") => Commands::Server,
            Some("monitor") => Commands::Monitor,
            Some("stop") => Commands::Stop,
            _ => {
                let agy_args = if args.first().map(|s| s.as_str()) == Some("--") {
                    args[1..].to_vec()
                } else {
                    args
                };
                Commands::Run(agy_args)
            }
        };

        Ok(Self { config, command })
    }
}
