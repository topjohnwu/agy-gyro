// SPDX-License-Identifier: MIT

use crate::app::{APP_SESSION_ID, APP_SESSION_LABEL, launch_app, resolve_app_target};
use crate::config::{Config, connect_host};
use crate::proxy::{DaemonStatusResponse, ShutdownResponse};
use crate::usage::{
    format_session_api_key, generate_session_uuid, is_pid_alive, sanitize_session_label,
};
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

fn gateway_env_vars<'a>(daemon_url: &'a str, api_key: &'a str) -> [(&'static str, &'a str); 3] {
    [
        ("AGY_LLM_GATEWAY_URL", daemon_url),
        ("AGY_LLM_GATEWAY_WIRE_PROTOCOL", "genai"),
        ("AGY_LLM_GATEWAY_API_KEY", api_key),
    ]
}

fn build_daemon_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(200))
        .timeout(Duration::from_secs(15))
        .tcp_nodelay(true)
        .build()
}

async fn try_fetch_daemon_status(
    client: &reqwest::Client,
    daemon_base_url: &str,
    refresh: bool,
) -> Result<DaemonStatusResponse, String> {
    let url = if refresh {
        format!(
            "{}/sessions?refresh=1",
            daemon_base_url.trim_end_matches('/')
        )
    } else {
        format!("{}/sessions", daemon_base_url.trim_end_matches('/'))
    };
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

fn spawn_background_daemon(cfg: &Config, port: u16) -> Result<(), Box<dyn std::error::Error>> {
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

fn build_id_matches(server_id: Option<&str>, client_id: &str) -> bool {
    server_id == Some(client_id)
}

async fn wait_for_daemon_stopped(
    client: &reqwest::Client,
    base_url: &str,
    pid: Option<u32>,
    timeout: Duration,
) -> bool {
    let probe_url = format!("{}/sessions", base_url.trim_end_matches('/'));
    let start = Instant::now();
    while start.elapsed() < timeout {
        let pid_done = pid.is_none_or(|p| p == std::process::id() || !is_pid_alive(p));
        let port_closed = client
            .get(&probe_url)
            .timeout(Duration::from_millis(100))
            .send()
            .await
            .is_err();
        if pid_done && port_closed {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

async fn force_kill_pid(pid: u32) {
    if pid == 0 || pid == std::process::id() || !is_pid_alive(pid) {
        return;
    }

    #[cfg(unix)]
    {
        unsafe {
            let _ = libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(500) && is_pid_alive(pid) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if is_pid_alive(pid) {
            unsafe {
                let _ = libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }

    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

async fn shutdown_daemon_for_restart(
    client: &reqwest::Client,
    base_url: &str,
    known_pid: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let shutdown_url = format!("{}/shutdown", base_url.trim_end_matches('/'));
    let pid = match client
        .post(&shutdown_url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => res
            .json::<ShutdownResponse>()
            .await
            .map(|p| p.pid)
            .unwrap_or(known_pid),
        _ => known_pid,
    };

    if wait_for_daemon_stopped(client, base_url, Some(pid), Duration::from_secs(2)).await {
        return Ok(());
    }

    force_kill_pid(pid).await;

    if wait_for_daemon_stopped(client, base_url, Some(pid), Duration::from_secs(2)).await {
        return Ok(());
    }

    Err(format!(
        "Timed out stopping mismatched agy-gyro daemon (pid={}) on {}",
        pid, base_url
    )
    .into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonLaunchOutcome {
    AlreadyRunning,
    Started,
    Restarted { old_pid: u32 },
}

/// Probes the configured daemon address; if unreachable, spawns the background daemon.
/// If a daemon is already running with a mismatched build ID, kills and restarts it automatically.
/// When `refresh_if_running` is `true` and an already-running daemon matches the current build ID,
/// triggers a deduplicated catalog & pricing refresh via `GET /sessions?refresh=1`.
async fn ensure_daemon_status(
    config: &Config,
    refresh_if_running: bool,
) -> Result<(String, DaemonStatusResponse, DaemonLaunchOutcome), Box<dyn std::error::Error>> {
    let client = build_daemon_client()?;

    let port = if config.port != 0 {
        config.port
    } else {
        let listener = std::net::TcpListener::bind((connect_host(&config.host), 0))?;
        listener.local_addr()?.port()
    };

    let daemon_url = format!("http://{}:{}", connect_host(&config.host), port);
    let expected_build_id = crate::BUILD_ID;

    let outcome = match try_fetch_daemon_status(&client, &daemon_url, false).await {
        Ok(status) if build_id_matches(status.build_id.as_deref(), expected_build_id) => {
            let status = if refresh_if_running {
                try_fetch_daemon_status(&client, &daemon_url, true)
                    .await
                    .unwrap_or(status)
            } else {
                status
            };
            return Ok((daemon_url, status, DaemonLaunchOutcome::AlreadyRunning));
        }
        Ok(status) => {
            let server_id = status
                .build_id
                .as_deref()
                .map(crate::short_build_id)
                .unwrap_or("unknown");
            eprintln!(
                "agy-gyro: server build id mismatch (server={}, client={}); restarting daemon (pid={})...",
                server_id,
                crate::short_build_id(expected_build_id),
                status.pid
            );
            shutdown_daemon_for_restart(&client, &daemon_url, status.pid).await?;
            DaemonLaunchOutcome::Restarted {
                old_pid: status.pid,
            }
        }
        Err(_) => DaemonLaunchOutcome::Started,
    };

    spawn_background_daemon(config, port)?;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        tokio::time::sleep(Duration::from_millis(40)).await;
        if let Ok(status) = try_fetch_daemon_status(&client, &daemon_url, false).await
            && build_id_matches(status.build_id.as_deref(), expected_build_id)
        {
            return Ok((daemon_url, status, outcome));
        }
    }

    Err(format!(
        "Timed out waiting for background agy-gyro daemon on {}",
        daemon_url
    )
    .into())
}

/// Ensures the background `agy-gyro` daemon is running on the configured port without blocking.
/// If a daemon is already running with a matching build ID, prints its status and returns immediately.
/// If a running daemon has a different build ID, kills and restarts it automatically.
pub async fn start_daemon(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let (daemon_url, status, outcome) = ensure_daemon_status(&config, false).await?;

    match outcome {
        DaemonLaunchOutcome::AlreadyRunning => {
            println!(
                "agy-gyro daemon (pid={}) is already running on {}",
                status.pid, daemon_url
            );
        }
        DaemonLaunchOutcome::Started => {
            println!(
                "Started agy-gyro daemon (pid={}) on {}",
                status.pid, daemon_url
            );
        }
        DaemonLaunchOutcome::Restarted { old_pid } => {
            println!(
                "Restarted agy-gyro daemon (old pid={}, new pid={}) on {}",
                old_pid, status.pid, daemon_url
            );
        }
    }
    Ok(())
}

/// If a daemon is already running on `config`'s port with a mismatched build ID and an API key
/// is configured, kills and restarts it so clients connect to the matching server build.
pub async fn restart_daemon_if_build_mismatch(
    config: &Config,
) -> Result<bool, Box<dyn std::error::Error>> {
    if config.port == 0 || config.api_key.trim().is_empty() {
        return Ok(false);
    }

    let client = build_daemon_client()?;
    let daemon_url = config.base_url();
    let Ok(status) = try_fetch_daemon_status(&client, &daemon_url, false).await else {
        return Ok(false);
    };

    if build_id_matches(status.build_id.as_deref(), crate::BUILD_ID) {
        return Ok(false);
    }

    let (_, _, outcome) = ensure_daemon_status(config, false).await?;
    Ok(matches!(outcome, DaemonLaunchOutcome::Restarted { .. }))
}

fn ensure_daemon_blocking(config: &Config) -> Result<String, Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (daemon_url, _, _) = rt.block_on(ensure_daemon_status(config, true))?;
    Ok(daemon_url)
}

/// Ensures the background `agy-gyro` daemon is running, configures the gateway environment
/// variables with a lazy session token (`gyro:<uuid>:<pid>:<workspace>`), and replaces the
/// current process in-place with `agy` (`execvp` on Unix) so no wrapper process remains.
pub fn run_wrapper(
    config: Config,
    agy_args: Vec<String>,
) -> Result<i32, Box<dyn std::error::Error>> {
    let daemon_url = ensure_daemon_blocking(&config)?;

    let session_id = generate_session_uuid();
    let label = current_workspace_label();
    let api_key = format_session_api_key(&session_id, Some(std::process::id()), &label);

    let mut cmd = std::process::Command::new(&config.agy_path);
    cmd.args(&agy_args)
        .env("AGY_ENABLE_HUB", "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (k, v) in gateway_env_vars(&daemon_url, &api_key) {
        cmd.env(k, v);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("agy-gyro: failed to execute '{}': {}", config.agy_path, err);
        Ok(127)
    }

    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(status) => Ok(status.code().unwrap_or(1)),
            Err(err) => {
                eprintln!("agy-gyro: failed to execute '{}': {}", config.agy_path, err);
                Ok(127)
            }
        }
    }
}

/// Ensures the background `agy-gyro` daemon is running and launches the Antigravity desktop app
/// detached with the gateway environment variables and fixed lazy session (`#app Antigravity`).
pub fn run_app(config: Config, app_args: Vec<String>) -> Result<i32, Box<dyn std::error::Error>> {
    let target = resolve_app_target(config.app_path.as_deref())?;
    let daemon_url = ensure_daemon_blocking(&config)?;
    let api_key = format_session_api_key(APP_SESSION_ID, None, APP_SESSION_LABEL);
    let env = gateway_env_vars(&daemon_url, &api_key);
    launch_app(&target, &env, &app_args)
}

/// Stops a running `agy-gyro` background daemon at the target address configured by `config`.
pub async fn stop_daemon(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let base_url = config.base_url();
    let shutdown_url = format!("{}/shutdown", base_url);
    let client = build_daemon_client()?;

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

    let _ = wait_for_daemon_stopped(&client, &base_url, pid, Duration::from_secs(3)).await;

    if let Some(p) = pid {
        println!("Stopped agy-gyro daemon (pid={}) at {}", p, base_url);
    } else {
        println!("Stopped agy-gyro daemon at {}", base_url);
    }
    Ok(())
}
