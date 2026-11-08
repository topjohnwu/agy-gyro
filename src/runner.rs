// SPDX-License-Identifier: MIT

use crate::config::WrapperArgs;
use crate::proxy::{RegisterSessionRequest, RegisterSessionResponse};
use crate::usage::sanitize_session_label;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

fn current_workspace_label() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .map(|s| sanitize_session_label(&s))
        .unwrap_or_else(|| "agy".to_string())
}

fn connect_host(bind_host: &str) -> &str {
    if bind_host == "0.0.0.0" {
        "127.0.0.1"
    } else {
        bind_host
    }
}

fn build_registration_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(200))
        .timeout(Duration::from_secs(15))
        .tcp_nodelay(true)
        .build()
}

async fn try_register_session(
    client: &reqwest::Client,
    daemon_base_url: &str,
    pid: Option<u32>,
    label: &str,
) -> Result<RegisterSessionResponse, String> {
    let url = format!("{}/sessions", daemon_base_url.trim_end_matches('/'));
    let res = client
        .post(&url)
        .json(&RegisterSessionRequest {
            pid,
            label: label.to_string(),
        })
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }

    res.json::<RegisterSessionResponse>()
        .await
        .map_err(|e| e.to_string())
}

fn is_agy_gyro_binary(exe_path: &Path) -> bool {
    exe_path
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|stem| stem == "agy-gyro" || stem == "agy_gyro")
}

fn resolve_daemon_exe() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    if is_agy_gyro_binary(&exe) {
        return Ok(exe);
    }

    let bin_name = if cfg!(windows) {
        "agy-gyro.exe"
    } else {
        "agy-gyro"
    };
    if let Some(candidate) = exe
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join(bin_name))
        && candidate.exists()
    {
        return Ok(candidate);
    }

    Err("Unable to locate agy-gyro executable to spawn background daemon".into())
}

fn spawn_background_daemon(
    wrapper_args: &WrapperArgs,
    port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let exe = resolve_daemon_exe()?;
    let cfg = &wrapper_args.config;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("server")
        .env("AGY_GYRO_HOST", &cfg.host)
        .env("AGY_GYRO_PORT", port.to_string())
        .env("AGY_GYRO_UPSTREAM_URL", &cfg.upstream)
        .env("AGY_GYRO_MAX_RETRIES", cfg.max_retries.to_string())
        .env(
            "AGY_GYRO_INITIAL_DELAY_MS",
            cfg.initial_delay_ms.to_string(),
        )
        .env("AGY_GYRO_MAX_DELAY_MS", cfg.max_delay_ms.to_string())
        .env("AGY_GYRO_NO_JITTER", cfg.no_jitter.to_string())
        .env("AGY_GYRO_NO_BUFFER", cfg.no_buffer.to_string())
        .env(
            "AGY_GYRO_REQUEST_TIMEOUT_SECS",
            cfg.request_timeout_secs.to_string(),
        )
        .env("GEMINI_API_KEY", &cfg.api_key)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if let Some(ref log_file) = wrapper_args.log_file {
        cmd.env("AGY_GYRO_LOG_FILE", log_file);
    }

    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            match libc::fork() {
                -1 => Err(std::io::Error::last_os_error()),
                0 => Ok(()),
                _ => libc::_exit(0),
            }
        });
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    #[cfg(unix)]
    {
        let _ = cmd.spawn()?.wait();
    }

    #[cfg(not(unix))]
    {
        let _ = cmd.spawn()?;
    }

    Ok(())
}

async fn ensure_daemon_and_register(
    wrapper_args: &WrapperArgs,
    pid: Option<u32>,
    label: &str,
) -> Result<(String, String, String, String), Box<dyn std::error::Error>> {
    let config = &wrapper_args.config;
    let client = build_registration_client()?;

    let port = if config.port != 0 {
        config.port
    } else {
        let listener = std::net::TcpListener::bind((connect_host(&config.host), 0))?;
        listener.local_addr()?.port()
    };

    let daemon_url = format!("http://{}:{}", connect_host(&config.host), port);

    let env_models = std::env::var("AGY_LLM_GATEWAY_MODELS")
        .ok()
        .filter(|s| !s.trim().is_empty());

    if let Ok(reg) = try_register_session(&client, &daemon_url, pid, label).await {
        let models = env_models.unwrap_or(reg.gateway_models);
        return Ok((daemon_url, reg.session_id, reg.api_key, models));
    }

    spawn_background_daemon(wrapper_args, port)?;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        tokio::time::sleep(Duration::from_millis(40)).await;
        if let Ok(reg) = try_register_session(&client, &daemon_url, pid, label).await {
            let models = env_models.unwrap_or(reg.gateway_models);
            return Ok((daemon_url, reg.session_id, reg.api_key, models));
        }
    }

    Err(format!(
        "Timed out waiting for background agy-gyro daemon on {}",
        daemon_url
    )
    .into())
}

fn cleanup_failed_session(daemon_url: &str, session_id: &str) {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    rt.block_on(async {
        if let Ok(client) = build_registration_client() {
            let url = format!(
                "{}/sessions/{}",
                daemon_url.trim_end_matches('/'),
                session_id
            );
            let _ = client
                .delete(&url)
                .timeout(Duration::from_millis(500))
                .send()
                .await;
        }
    });
}

/// Ensures the background `agy-gyro` daemon is running, registers a session for this `agy`
/// instance, configures the gateway environment variables, and replaces the current process
/// in-place with `agy` (`execvp` on Unix) so no wrapper process remains.
pub fn run_wrapper(wrapper_args: WrapperArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let pid = Some(std::process::id());
    let label = current_workspace_label();

    let (daemon_url, session_id, session_api_key, gateway_models) = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(ensure_daemon_and_register(&wrapper_args, pid, &label))?
    };

    let mut cmd = std::process::Command::new(&wrapper_args.agy_path);
    cmd.args(&wrapper_args.agy_args)
        .env("AGY_LLM_GATEWAY_URL", &daemon_url)
        .env("AGY_LLM_GATEWAY_WIRE_PROTOCOL", "genai")
        .env("AGY_LLM_GATEWAY_API_KEY", &session_api_key)
        .env("AGY_LLM_GATEWAY_MODELS", &gateway_models)
        .env("AGY_ENABLE_HUB", "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        cleanup_failed_session(&daemon_url, &session_id);
        eprintln!(
            "agy-gyro: failed to execute '{}': {}",
            wrapper_args.agy_path, err
        );
        Ok(127)
    }

    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(status) => Ok(status.code().unwrap_or(1)),
            Err(err) => {
                cleanup_failed_session(&daemon_url, &session_id);
                eprintln!(
                    "agy-gyro: failed to execute '{}': {}",
                    wrapper_args.agy_path, err
                );
                Ok(127)
            }
        }
    }
}
