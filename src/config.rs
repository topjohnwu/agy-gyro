// SPDX-License-Identifier: MIT

use serde::Deserialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub use crate::models::{DEFAULT_GATEWAY_MODELS, EXCLUDED_MODELS, MODEL_EFFORT_LEVELS};

/// Default TCP port for the local gateway server and monitor (`6470` = `GYRO` in leetspeak).
pub const DEFAULT_PORT: u16 = 6470;

#[derive(Debug, Clone)]
pub struct Cli {
    pub command: Option<Commands>,
    pub wrapper_args: WrapperArgs,
}

/// Configuration loaded from `~/.gemini/config/agy-gyro.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FileConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub upstream: Option<String>,
    #[serde(alias = "api-key")]
    pub api_key: Option<String>,
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

/// Returns the default configuration file path: `~/.gemini/config/agy-gyro.toml`.
pub fn default_config_path() -> Option<PathBuf> {
    std::env::home_dir().map(|home| home.join(".gemini").join("config").join("agy-gyro.toml"))
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

        let host = env_non_empty("AGY_GYRO_HOST")
            .or(file_config.host)
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let port = env_parse::<u16>("AGY_GYRO_PORT")?
            .or(file_config.port)
            .unwrap_or(DEFAULT_PORT);
        let upstream = env_non_empty("AGY_GYRO_UPSTREAM")
            .or_else(|| env_non_empty("AGY_GYRO_UPSTREAM_URL"))
            .or(file_config.upstream)
            .unwrap_or_else(|| "https://generativelanguage.googleapis.com".to_string());
        let max_retries = env_parse::<u32>("AGY_GYRO_MAX_RETRIES")?
            .or(file_config.max_retries)
            .unwrap_or(15);
        let initial_delay_ms = env_parse::<u64>("AGY_GYRO_INITIAL_DELAY_MS")?
            .or(file_config.initial_delay_ms)
            .unwrap_or(1000);
        let max_delay_ms = env_parse::<u64>("AGY_GYRO_MAX_DELAY_MS")?
            .or(file_config.max_delay_ms)
            .unwrap_or(60000);
        let no_jitter = env_bool("AGY_GYRO_NO_JITTER")?
            .or(file_config.no_jitter)
            .unwrap_or(false);
        let no_buffer = env_bool("AGY_GYRO_NO_BUFFER")?
            .or(file_config.no_buffer)
            .unwrap_or(false);
        let request_timeout_secs = env_parse::<u64>("AGY_GYRO_REQUEST_TIMEOUT_SECS")?
            .or(file_config.request_timeout_secs)
            .unwrap_or(600);
        let api_key = env_non_empty("GEMINI_API_KEY")
            .or_else(|| env_non_empty("AGY_GYRO_API_KEY"))
            .or(file_config.api_key)
            .unwrap_or_default();
        let agy_path = env_non_empty("AGY_GYRO_AGY_PATH")
            .or(file_config.agy_path)
            .unwrap_or_else(|| "agy".to_string());
        let log_file = env_non_empty("AGY_GYRO_LOG_FILE")
            .map(PathBuf::from)
            .or(file_config.log_file);
        let url = env_non_empty("AGY_GYRO_URL");

        let config = Config {
            host: host.clone(),
            port,
            upstream,
            max_retries,
            initial_delay_ms,
            max_delay_ms,
            no_jitter,
            no_buffer,
            request_timeout_secs,
            api_key,
        };

        let command = match args.first().map(|s| s.as_str()) {
            Some("server") => Some(Commands::Server(ServerArgs {
                config: config.clone(),
                log_file: log_file.clone(),
            })),
            Some("monitor") => Some(Commands::Monitor(MonitorArgs {
                url: url.clone(),
                host: host.clone(),
                port,
            })),
            Some("stop") => Some(Commands::Stop(MonitorArgs { url, host, port })),
            _ => None,
        };

        let agy_args = if command.is_some() {
            Vec::new()
        } else if args.first().map(|s| s.as_str()) == Some("--") {
            args[1..].to_vec()
        } else {
            args
        };

        let wrapper_args = WrapperArgs {
            config,
            agy_path,
            log_file,
            agy_args,
        };

        let requires_api_key = matches!(
            command,
            Some(Commands::Server(_)) | Some(Commands::Run(_)) | None
        );

        if requires_api_key && wrapper_args.config.api_key.trim().is_empty() {
            return Err(
                "missing Gemini API key: set GEMINI_API_KEY or configure `api_key` in ~/.gemini/config/agy-gyro.toml"
                    .to_string(),
            );
        }

        Ok(Self {
            command,
            wrapper_args,
        })
    }
}

#[derive(Debug, Clone)]
pub enum Commands {
    /// Start the background agy-gyro daemon (if not already running)
    Server(ServerArgs),
    /// Run agy CLI wrapped with the local proxy (default mode)
    Run(WrapperArgs),
    /// Monitor live token usage and cost in a terminal UI dashboard
    Monitor(MonitorArgs),
    /// Stop the running background agy-gyro daemon
    Stop(MonitorArgs),
}

#[derive(Debug, Clone)]
pub struct MonitorArgs {
    /// Target agy-gyro gateway URL or /usage endpoint URL
    pub url: Option<String>,
    /// Host address of the local agy-gyro server
    pub host: String,
    /// Port of the local agy-gyro server
    pub port: u16,
}

impl MonitorArgs {
    pub fn resolve_base_url(&self) -> String {
        if let Some(ref u) = self.url {
            let trimmed = u.trim().trim_end_matches('/');
            let base = trimmed
                .strip_suffix("/usage")
                .or_else(|| trimmed.strip_suffix("/logs"))
                .unwrap_or(trimmed);
            if base.starts_with("http://") || base.starts_with("https://") {
                base.to_string()
            } else {
                format!("http://{}", base)
            }
        } else {
            let host = if self.host == "0.0.0.0" {
                "127.0.0.1"
            } else {
                &self.host
            };
            format!("http://{}:{}", host, self.port)
        }
    }

    pub fn resolve_endpoint_url(&self) -> String {
        if let Some(ref u) = self.url {
            crate::tui::resolve_usage_endpoint_url(u)
        } else {
            format!("{}/usage", self.resolve_base_url())
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerArgs {
    pub config: Config,
    /// Optional log file path for agy-gyro proxy tracing logs
    pub log_file: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct WrapperArgs {
    pub config: Config,
    /// Executable path or command name for Antigravity CLI
    pub agy_path: String,
    /// Optional log file path for agy-gyro proxy tracing logs
    pub log_file: Option<PathBuf>,
    /// Raw arguments passed through directly to agy
    pub agy_args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Host address to bind the proxy server to
    pub host: String,
    /// Port to listen on
    pub port: u16,
    /// Upstream Gemini API base URL
    pub upstream: String,
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
    /// Gemini API key for authenticating upstream requests
    pub api_key: String,
}

impl Config {
    pub fn is_jitter_enabled(&self) -> bool {
        !self.no_jitter
    }

    pub fn is_buffer_enabled(&self) -> bool {
        !self.no_buffer
    }
}
