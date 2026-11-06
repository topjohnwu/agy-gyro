// SPDX-License-Identifier: MIT

use agy_gyro::config::{Cli, Commands, ServerArgs};
use agy_gyro::proxy::{ProxyState, create_router};
use agy_gyro::runner::run_wrapper;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::load();

    match cli.command {
        Some(Commands::Server(server_args)) => run_server(server_args).await,
        Some(Commands::Run(wrapper_args)) => {
            let code = run_wrapper(wrapper_args).await?;
            std::process::exit(code);
        }
        None => {
            let code = run_wrapper(cli.wrapper_args).await?;
            std::process::exit(code);
        }
    }
}

async fn run_server(server_args: ServerArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing subscriber with env filter
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,agy_gyro=debug"));

    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .try_init();

    let port = server_args.resolved_port();
    let mut config = server_args.config;
    config.port = Some(port);

    info!("Starting agy-gyro local Antigravity Enterprise Gateway server");
    info!("Upstream URL: {}", config.upstream);
    info!(
        "Retry configuration: max_retries={}, initial_delay_ms={}ms, max_delay_ms={}ms, jitter={}",
        config.max_retries,
        config.initial_delay_ms,
        config.max_delay_ms,
        config.is_jitter_enabled()
    );

    let client = agy_gyro::proxy::build_http_client(&config)?;
    let gateway_models = agy_gyro::proxy::resolve_gateway_models(&client, &config).await;
    let state = Arc::new(ProxyState::new(config.clone(), client));

    let app = create_router(state);

    let addr = format!("{}:{}", config.host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    let model_list: Vec<_> = gateway_models
        .split(',')
        .filter(|m| !m.is_empty())
        .map(|m| format!("      {{ \"modelId\": \"{}\" }}", m))
        .collect();

    info!("agy-gyro gateway listening on http://{}", addr);
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
