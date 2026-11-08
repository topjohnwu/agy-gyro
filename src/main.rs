// SPDX-License-Identifier: MIT

use agy_gyro::config::{Cli, Commands, ServerArgs};
use agy_gyro::proxy::{ProxyState, create_router};
use agy_gyro::runner::run_wrapper;
use agy_gyro::tui::run_monitor;
use agy_gyro::usage::{DEFAULT_PRICING_URL, UsageLogLayer, fetch_model_pricing};
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::load();

    match cli.command {
        Some(Commands::Server(server_args)) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(run_server(server_args))
        }
        Some(Commands::Monitor(monitor_args)) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(run_monitor(monitor_args))
        }
        Some(Commands::Run(wrapper_args)) => {
            let code = run_wrapper(wrapper_args)?;
            std::process::exit(code);
        }
        None => {
            let code = run_wrapper(cli.wrapper_args)?;
            std::process::exit(code);
        }
    }
}

async fn run_server(server_args: ServerArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing subscriber with env filter
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,agy_gyro=debug"));

    if let Some(ref path) = server_args.log_file {
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
            .with(UsageLogLayer)
            .try_init();
    } else {
        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer())
            .with(UsageLogLayer)
            .try_init();
    }

    let config = server_args.config;

    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let bound_addr = listener.local_addr()?;

    let client = agy_gyro::proxy::build_http_client(&config)?;
    let state = Arc::new(ProxyState::new(config.clone(), client.clone()));

    info!("Starting agy-gyro local Antigravity Enterprise Gateway server");
    info!("Upstream URL: {}", config.upstream);
    info!(
        "Retry configuration: max_retries={}, initial_delay_ms={}ms, max_delay_ms={}ms, jitter={}",
        config.max_retries,
        config.initial_delay_ms,
        config.max_delay_ms,
        config.is_jitter_enabled()
    );

    let env_models = std::env::var("AGY_LLM_GATEWAY_MODELS")
        .ok()
        .filter(|s| !s.trim().is_empty());

    let (gateway_models, pricing) = match env_models {
        Some(models) => {
            let pricing = fetch_model_pricing(&client, DEFAULT_PRICING_URL).await;
            (models, pricing)
        }
        None => tokio::join!(
            agy_gyro::proxy::resolve_gateway_models(&client, &config),
            fetch_model_pricing(&client, DEFAULT_PRICING_URL)
        ),
    };
    state.set_gateway_models(gateway_models.clone());
    state.usage.set_pricing(pricing);

    let app = create_router(state);

    let model_list: Vec<_> = gateway_models
        .split(',')
        .filter(|m| !m.is_empty())
        .map(|m| format!("      {{ \"modelId\": \"{}\" }}", m))
        .collect();

    info!("agy-gyro gateway listening on http://{}", bound_addr);
    info!(
        "Live usage endpoint: http://{}/usage (run `agy-gyro monitor` for live TUI dashboard)",
        bound_addr
    );
    info!(
        "To configure via environment variables:\n  export AGY_LLM_GATEWAY_URL=http://{}\n  export AGY_LLM_GATEWAY_WIRE_PROTOCOL=genai\n  export AGY_LLM_GATEWAY_API_KEY=agy-gyro\n  export AGY_LLM_GATEWAY_MODELS=\"{}\"\n",
        addr, gateway_models
    );
    info!(
        "To configure via ~/.gemini/config/config.json:\n{{\n  \"gateway\": {{\n    \"enabled\": true,\n    \"url\": \"http://{}\",\n    \"apiKey\": \"agy-gyro\",\n    \"wireProtocol\": \"genai\"\n  }},\n  \"models\": {{\n    \"list\": [\n{}\n    ]\n  }}\n}}\n",
        addr,
        model_list.join(",\n")
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("agy-gyro server stopped gracefully.");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutdown signal received, shutting down gracefully...");
}
