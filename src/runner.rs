// SPDX-License-Identifier: MIT

use crate::config::{Config, connect_host};
use crate::proxy::{
    DaemonStatusResponse, RegisterSessionRequest, RegisterSessionResponse, ShutdownResponse,
};
use crate::usage::{is_pid_alive, sanitize_session_label};
use std::future::Future;
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

async fn try_fetch_daemon_status(
    client: &reqwest::Client,
    daemon_base_url: &str,
) -> Result<DaemonStatusResponse, String> {
    let url = format!("{}/sessions", daemon_base_url.trim_end_matches('/'));
    let res = client.get(&url).send().await.map_err(|e| e.to_string())?;

    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }

    res.json::<DaemonStatusResponse>()
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
    cfg: &Config,
    port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let exe = resolve_daemon_exe()?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("server")
        .env("AGY_GYRO_DAEMON_WORKER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    cfg.apply_env_to_command(&mut cmd, port);

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

/// Probes the configured daemon address; if unreachable, spawns the background daemon
/// and polls `probe` until it succeeds or times out after 15 seconds.
/// Returns `(daemon_url, probe_result, was_already_running)`.
async fn ensure_daemon_ready<T, F, Fut>(
    config: &Config,
    mut probe: F,
) -> Result<(String, T, bool), Box<dyn std::error::Error>>
where
    F: FnMut(reqwest::Client, String) -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let client = build_registration_client()?;

    let port = if config.port != 0 {
        config.port
    } else {
        let listener = std::net::TcpListener::bind((connect_host(&config.host), 0))?;
        listener.local_addr()?.port()
    };

    let daemon_url = format!("http://{}:{}", connect_host(&config.host), port);

    if let Ok(res) = probe(client.clone(), daemon_url.clone()).await {
        return Ok((daemon_url, res, true));
    }

    spawn_background_daemon(config, port)?;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        tokio::time::sleep(Duration::from_millis(40)).await;
        if let Ok(res) = probe(client.clone(), daemon_url.clone()).await {
            return Ok((daemon_url, res, false));
        }
    }

    Err(format!(
        "Timed out waiting for background agy-gyro daemon on {}",
        daemon_url
    )
    .into())
}

/// Ensures the background `agy-gyro` daemon is running on the configured port without blocking.
/// If a daemon is already running on the configured port, prints its status and returns immediately.
pub async fn start_daemon(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let (daemon_url, status, already_running) =
        ensure_daemon_ready(&config, |client, url| async move {
            try_fetch_daemon_status(&client, &url).await
        })
        .await?;

    if already_running {
        println!(
            "agy-gyro daemon (pid={}) is already running on {}",
            status.pid, daemon_url
        );
    } else {
        println!(
            "Started agy-gyro daemon (pid={}) on {}",
            status.pid, daemon_url
        );
    }
    Ok(())
}

async fn ensure_daemon_and_register(
    config: &Config,
    pid: Option<u32>,
    label: &str,
) -> Result<(String, String, String), Box<dyn std::error::Error>> {
    let label_owned = label.to_string();
    let (daemon_url, reg, _) = ensure_daemon_ready(config, move |client, url| {
        let lbl = label_owned.clone();
        async move { try_register_session(&client, &url, pid, &lbl).await }
    })
    .await?;

    Ok((daemon_url, reg.session_id, reg.api_key))
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
pub fn run_wrapper(config: Config, agy_args: Vec<String>) -> Result<i32, Box<dyn std::error::Error>> {
    let pid = Some(std::process::id());
    let label = current_workspace_label();

    let (daemon_url, session_id, session_api_key) = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(ensure_daemon_and_register(&config, pid, &label))?
    };

    let mut cmd = std::process::Command::new(&config.agy_path);
    cmd.args(&agy_args)
        .env("AGY_LLM_GATEWAY_URL", &daemon_url)
        .env("AGY_LLM_GATEWAY_WIRE_PROTOCOL", "genai")
        .env("AGY_LLM_GATEWAY_API_KEY", &session_api_key)
        .env("AGY_ENABLE_HUB", "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        cleanup_failed_session(&daemon_url, &session_id);
        eprintln!("agy-gyro: failed to execute '{}': {}", config.agy_path, err);
        Ok(127)
    }

    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(status) => Ok(status.code().unwrap_or(1)),
            Err(err) => {
                cleanup_failed_session(&daemon_url, &session_id);
                eprintln!("agy-gyro: failed to execute '{}': {}", config.agy_path, err);
                Ok(127)
            }
        }
    }
}

/// Stops a running `agy-gyro` background daemon at the target address configured by `config`.
pub async fn stop_daemon(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let base_url = config.base_url();
    let shutdown_url = format!("{}/shutdown", base_url);
    let client = build_registration_client()?;

    let res = match client
        .post(&shutdown_url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
    {
        Ok(res) => res,
        Err(err) if err.is_connect() => {
            println!("No running agy-gyro daemon found at {}", base_url);
            return Ok(());
        }
        Err(err) => {
            return Err(
                format!("Failed to contact agy-gyro daemon at {}: {}", base_url, err).into(),
            );
        }
    };

    if !res.status().is_success() {
        return Err(format!(
            "Daemon at {} returned unexpected status {}",
            base_url,
            res.status()
        )
        .into());
    }

    let payload = res.json::<ShutdownResponse>().await.ok();
    let pid = payload.as_ref().map(|p| p.pid);

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        let pid_done = pid.is_none_or(|p| p == std::process::id() || !is_pid_alive(p));
        let port_closed = client
            .get(config.usage_url())
            .timeout(Duration::from_millis(100))
            .send()
            .await
            .is_err();
        if pid_done && port_closed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    if let Some(p) = pid {
        println!("Stopped agy-gyro daemon (pid={}) at {}", p, base_url);
    } else {
        println!("Stopped agy-gyro daemon at {}", base_url);
    }
    Ok(())
}
