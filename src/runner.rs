// SPDX-License-Identifier: MIT

use crate::config::WrapperArgs;
use crate::proxy::{ProxyState, build_http_client, create_router, resolve_gateway_models};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Initializes the tracing subscriber for wrapper mode.
///
/// In wrapper mode, to enforce the strict Zero-TTY Output Rule:
/// - If `log_file` is provided, logs are written exclusively to that file.
/// - If `log_file` is `None`, logs are directed to `std::io::sink()` (completely silenced).
pub fn init_wrapper_tracing(log_file: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,agy_gyro=debug"));

    if let Some(path) = log_file {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(file)
                    .with_ansi(false),
            )
            .try_init();
    } else {
        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::sink)
                    .with_ansi(false),
            )
            .try_init();
    }

    Ok(())
}

/// Executes agy wrapped with an in-process agy-gyro proxy server and returns the process exit code.
pub async fn run_wrapper(wrapper_args: WrapperArgs) -> Result<i32, Box<dyn std::error::Error>> {
    init_wrapper_tracing(wrapper_args.log_file.as_deref())?;

    let mut config = wrapper_args.config.clone();
    let port = wrapper_args.resolved_port();
    config.port = Some(port);

    // Bind listener to dynamic port (port 0 by default) or user-specified port
    let addr = format!("{}:{}", config.host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let bound_addr = listener.local_addr()?;

    info!(
        "agy-gyro wrapper gateway listening on http://{}",
        bound_addr
    );

    let client = build_http_client(&config)?;

    let gateway_models = match std::env::var("AGY_LLM_GATEWAY_MODELS")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        Some(models) => models,
        None => resolve_gateway_models(&client, &config).await,
    };

    let state = Arc::new(ProxyState::new(config, client));
    let app = create_router(state);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
    });

    let proxy_url = format!("http://{}", bound_addr);

    let mut cmd = tokio::process::Command::new(&wrapper_args.agy_path);
    cmd.args(&wrapper_args.agy_args)
        .env("AGY_LLM_GATEWAY_URL", &proxy_url)
        .env("AGY_LLM_GATEWAY_WIRE_PROTOCOL", "genai")
        .env("AGY_LLM_GATEWAY_API_KEY", "agy-gyro")
        .env("AGY_LLM_GATEWAY_MODELS", &gateway_models)
        .env("AGY_ENABLE_HUB", "1")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            error!(
                "Failed to execute target binary '{}': {}",
                wrapper_args.agy_path, err
            );
            let _ = shutdown_tx.send(());
            let _ = server_handle.await;
            eprintln!(
                "agy-gyro: failed to execute '{}': {}",
                wrapper_args.agy_path, err
            );
            return Ok(127);
        }
    };

    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(unix)]
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;

    #[cfg(windows)]
    let mut ctrl_c = tokio::signal::windows::ctrl_c()?;

    let exit_status = tokio::select! {
        res = child.wait() => {
            res?
        }
        sig = async {
            #[cfg(unix)]
            tokio::select! {
                _ = sigterm.recv() => libc::SIGTERM,
                _ = sighup.recv() => libc::SIGHUP,
            }
            #[cfg(windows)]
            {
                let _ = ctrl_c.recv().await;
                0
            }
            #[cfg(not(any(unix, windows)))]
            std::future::pending::<i32>().await
        } => {
            if let Some(pid) = child.id() {
                #[cfg(unix)]
                unsafe {
                    libc::kill(pid as libc::pid_t, sig);
                }
                #[cfg(windows)]
                {
                    let _ = child.kill().await;
                }
            }

            match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                Ok(res) => res?,
                Err(_) => {
                    let _ = child.kill().await;
                    child.wait().await?
                }
            }
        }
    };

    let _ = shutdown_tx.send(());
    let _ = server_handle.await;

    let exit_code = exit_status.code().unwrap_or_else(|| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(sig) = exit_status.signal() {
                return 128 + sig;
            }
        }
        1
    });

    Ok(exit_code)
}
