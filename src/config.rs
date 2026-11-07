// SPDX-License-Identifier: MIT

use clap::parser::ValueSource;
use clap::{ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde::Deserialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Default TCP port for the local gateway server and monitor (`6470` = `GYRO` in leetspeak).
pub const DEFAULT_PORT: u16 = 6470;

/// Fallback list of Gemini models if dynamic discovery fails.
pub const DEFAULT_GATEWAY_MODELS: &[&str] = &["gemini-4-argon", "gemini-3.8-flash"];

/// Effort suffixes expanded for each thinking-capable Gemini model.
pub const MODEL_EFFORT_LEVELS: &[&str] = &["low", "medium", "high"];

/// Fixed list of older, lite, alias, or specialized-variant Core Gemini Text / Reasoning
/// models to exclude when dynamically building the model list from `/v1beta/models`.
pub const EXCLUDED_MODELS: &[&str] = &[
    "gemini-flash-latest",
    "gemini-flash-lite-latest",
    "gemini-pro-latest",
    "gemini-2.5-flash-lite",
    "gemini-3-flash-preview",
    "gemini-3.1-pro-preview",
    "gemini-3.1-pro-preview-customtools",
    "gemini-3.1-flash-lite-preview",
    "gemini-3.1-flash-lite",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-3.6-flash",
    "gemini-3.7-flash",
];

#[derive(Parser, Debug, Clone)]
#[command(
    name = "agy-gyro",
    author,
    version,
    about = "Local Antigravity Enterprise Gateway and Gemini API retry proxy for Antigravity CLI (agy)",
    subcommand_negates_reqs = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    #[command(flatten)]
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

fn is_explicit_source(matches: &ArgMatches, id: &str) -> bool {
    matches!(
        matches.value_source(id),
        Some(ValueSource::CommandLine | ValueSource::EnvVariable)
    )
}

impl Cli {
    /// Parses CLI arguments, environment variables, and `~/.gemini/config/agy-gyro.toml`,
    /// exiting on error.
    pub fn load() -> Self {
        Self::try_parse_with_config_from(std::env::args_os(), default_config_path().as_deref())
            .unwrap_or_else(|e| e.exit())
    }

    /// Parses from an iterator of arguments along with an optional TOML configuration file path.
    ///
    /// Precedence order (highest to lowest):
    /// 1. Command-line flags
    /// 2. Environment variables
    /// 3. Configuration file (`~/.gemini/config/agy-gyro.toml`)
    /// 4. Built-in default values
    pub fn try_parse_with_config_from<I, T>(
        itr: I,
        config_path: Option<&Path>,
    ) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let matches = Self::command().try_get_matches_from(itr)?;
        let mut cli = Self::from_arg_matches(&matches)?;

        let file_config = match config_path {
            Some(path) => FileConfig::load_from_path(path).map_err(|msg| {
                Self::command().error(clap::error::ErrorKind::ValueValidation, msg)
            })?,
            None => None,
        };

        if let Some(ref fc) = file_config {
            match cli.command {
                Some(Commands::Server(ref mut server_args)) => {
                    let sub_matches = matches.subcommand_matches("server").unwrap_or(&matches);
                    server_args.config.apply_file_config(sub_matches, fc);
                }
                Some(Commands::Run(ref mut wrapper_args)) => {
                    let sub_matches = matches.subcommand_matches("run").unwrap_or(&matches);
                    wrapper_args.apply_file_config(sub_matches, fc);
                }
                Some(Commands::Monitor(ref mut monitor_args)) => {
                    let sub_matches = matches.subcommand_matches("monitor").unwrap_or(&matches);
                    monitor_args.apply_file_config(sub_matches, fc);
                }
                None => {
                    cli.wrapper_args.apply_file_config(&matches, fc);
                }
            }
        }

        let active_config = match &cli.command {
            Some(Commands::Server(s)) => Some(&s.config),
            Some(Commands::Run(w)) => Some(&w.config),
            Some(Commands::Monitor(_)) => None,
            None => Some(&cli.wrapper_args.config),
        };

        if let Some(cfg) = active_config
            && cfg.api_key.trim().is_empty()
        {
            return Err(Self::command().error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "the following required arguments were not provided:\n  --api-key <API_KEY> (via --api-key, GEMINI_API_KEY, or ~/.gemini/config/agy-gyro.toml)",
            ));
        }

        Ok(cli)
    }
}

#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// Run standalone proxy server in the foreground
    Server(ServerArgs),
    /// Run agy CLI wrapped with the local proxy (default mode)
    Run(WrapperArgs),
    /// Monitor live token usage and cost in a terminal UI dashboard
    Monitor(MonitorArgs),
}

#[derive(Args, Debug, Clone)]
pub struct MonitorArgs {
    /// Target agy-gyro gateway URL or /usage endpoint URL
    #[arg(short = 'u', long, env = "AGY_GYRO_URL")]
    pub url: Option<String>,

    /// Host address of the local agy-gyro server
    #[arg(short = 'H', long, env = "AGY_GYRO_HOST", default_value = "127.0.0.1")]
    pub host: String,

    /// Port of the local agy-gyro server
    #[arg(short = 'p', long, env = "AGY_GYRO_PORT", default_value_t = DEFAULT_PORT)]
    pub port: u16,
}

impl MonitorArgs {
    pub fn apply_file_config(&mut self, matches: &ArgMatches, file: &FileConfig) {
        if !is_explicit_source(matches, "host")
            && let Some(ref v) = file.host
        {
            self.host = v.clone();
        }
        if !is_explicit_source(matches, "port")
            && let Some(v) = file.port
        {
            self.port = v;
        }
    }

    pub fn resolve_endpoint_url(&self) -> String {
        if let Some(ref u) = self.url {
            crate::tui::resolve_usage_endpoint_url(u)
        } else {
            let host = if self.host == "0.0.0.0" {
                "127.0.0.1"
            } else {
                &self.host
            };
            format!("http://{}:{}/usage", host, self.port)
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct ServerArgs {
    #[command(flatten)]
    pub config: Config,
}

#[derive(Args, Debug, Clone)]
pub struct WrapperArgs {
    #[command(flatten)]
    pub config: Config,

    /// Executable path or command name for Antigravity CLI
    #[arg(long, env = "AGY_GYRO_AGY_PATH", default_value = "agy")]
    pub agy_path: String,

    /// Optional log file path for agy-gyro proxy tracing logs
    #[arg(long, env = "AGY_GYRO_LOG_FILE")]
    pub log_file: Option<PathBuf>,

    /// Raw arguments passed through directly to agy
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub agy_args: Vec<String>,
}

impl WrapperArgs {
    pub fn apply_file_config(&mut self, matches: &ArgMatches, file: &FileConfig) {
        self.config.apply_file_config(matches, file);
        if !is_explicit_source(matches, "agy_path")
            && let Some(ref v) = file.agy_path
        {
            self.agy_path = v.clone();
        }
        if !is_explicit_source(matches, "log_file") && self.log_file.is_none() {
            self.log_file = file.log_file.clone();
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct Config {
    /// Host address to bind the proxy server to
    #[arg(short = 'H', long, env = "AGY_GYRO_HOST", default_value = "127.0.0.1")]
    pub host: String,

    /// Port to listen on
    #[arg(short = 'p', long, env = "AGY_GYRO_PORT", default_value_t = DEFAULT_PORT)]
    pub port: u16,

    /// Upstream Gemini API base URL
    #[arg(
        short = 'u',
        long,
        env = "AGY_GYRO_UPSTREAM_URL",
        default_value = "https://generativelanguage.googleapis.com"
    )]
    pub upstream: String,

    /// Maximum retry attempts on retriable errors
    #[arg(long, env = "AGY_GYRO_MAX_RETRIES", default_value_t = 15)]
    pub max_retries: u32,

    /// Initial retry backoff delay in milliseconds
    #[arg(long, env = "AGY_GYRO_INITIAL_DELAY_MS", default_value_t = 1000)]
    pub initial_delay_ms: u64,

    /// Maximum retry backoff delay in milliseconds
    #[arg(long, env = "AGY_GYRO_MAX_DELAY_MS", default_value_t = 60000)]
    pub max_delay_ms: u64,

    /// Disable jitter in exponential backoff calculation
    #[arg(long, env = "AGY_GYRO_NO_JITTER", default_value_t = false)]
    pub no_jitter: bool,

    /// Disable full stream chunk buffering (stream chunks immediately to client after first chunk)
    #[arg(long, env = "AGY_GYRO_NO_BUFFER", default_value_t = false)]
    pub no_buffer: bool,

    /// Client request timeout in seconds (for long thinking / generation requests)
    #[arg(long, env = "AGY_GYRO_REQUEST_TIMEOUT_SECS", default_value_t = 600)]
    pub request_timeout_secs: u64,

    /// Gemini API key for authenticating upstream requests
    #[arg(
        long,
        env = "GEMINI_API_KEY",
        hide_env_values = true,
        default_value = "",
        hide_default_value = true
    )]
    pub api_key: String,
}

impl Config {
    pub fn is_jitter_enabled(&self) -> bool {
        !self.no_jitter
    }

    pub fn is_buffer_enabled(&self) -> bool {
        !self.no_buffer
    }

    pub fn apply_file_config(&mut self, matches: &ArgMatches, file: &FileConfig) {
        if !is_explicit_source(matches, "host")
            && let Some(ref v) = file.host
        {
            self.host = v.clone();
        }
        if !is_explicit_source(matches, "port")
            && let Some(v) = file.port
        {
            self.port = v;
        }
        if !is_explicit_source(matches, "upstream")
            && let Some(ref v) = file.upstream
        {
            self.upstream = v.clone();
        }
        if !is_explicit_source(matches, "max_retries")
            && let Some(v) = file.max_retries
        {
            self.max_retries = v;
        }
        if !is_explicit_source(matches, "initial_delay_ms")
            && let Some(v) = file.initial_delay_ms
        {
            self.initial_delay_ms = v;
        }
        if !is_explicit_source(matches, "max_delay_ms")
            && let Some(v) = file.max_delay_ms
        {
            self.max_delay_ms = v;
        }
        if !is_explicit_source(matches, "no_jitter")
            && let Some(v) = file.no_jitter
        {
            self.no_jitter = v;
        }
        if !is_explicit_source(matches, "no_buffer")
            && let Some(v) = file.no_buffer
        {
            self.no_buffer = v;
        }
        if !is_explicit_source(matches, "request_timeout_secs")
            && let Some(v) = file.request_timeout_secs
        {
            self.request_timeout_secs = v;
        }
        if (!is_explicit_source(matches, "api_key") || self.api_key.trim().is_empty())
            && let Some(ref v) = file.api_key
        {
            self.api_key = v.clone();
        }
    }
}
