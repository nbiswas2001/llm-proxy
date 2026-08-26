mod config;
mod proxy;

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::{Router, routing::get};
use clap::Parser;
use tower_http::cors::CorsLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

/// OpenAI-compatible proxy in front of an online LLM API.
#[derive(Parser, Debug)]
#[command(name = "llm-proxy", version, about, long_about = None)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(short, long, default_value = "config.toml", env = "LLM_PROXY_CONFIG")]
    config: PathBuf,
    /// Override the port from the config file.
    #[arg(short, long)]
    port: Option<u16>,
    /// Override the bind address from the config file.
    #[arg(long)]
    host: Option<String>,
    /// Validate the config and exit without binding a port.
    #[arg(long)]
    check: bool,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("LLM_PROXY_LOG")
                .unwrap_or_else(|_| EnvFilter::new("llm_proxy=info,tower_http=warn")),
        )
        .init();

    let cli = Cli::parse();
    if !cli.config.exists() {
        anyhow::bail!(
            "config file {} not found - copy config.example.toml to it and add your API key",
            cli.config.display()
        );
    }
    let mut config = Config::load(&cli.config)?;
    if let Some(port) = cli.port {
        config.server.port = port;
    }
    if let Some(host) = cli.host {
        config.server.host = host;
    }

    for (name, provider) in &config.providers {
        let default = if config.is_default_provider(name) {
            " (default)"
        } else {
            ""
        };
        info!(
            "provider \"{name}\"{default} -> {} [api key: {}]",
            provider.base_url,
            if provider.api_key.is_some() {
                "yes"
            } else {
                "none"
            }
        );
    }

    if cli.check {
        info!("config OK: {}", cli.config.display());
        return Ok(());
    }

    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(config.server.connect_timeout_secs))
        .timeout(Duration::from_secs(config.server.request_timeout_secs))
        // Streaming responses must not be buffered or coalesced on the way out.
        .pool_idle_timeout(Duration::from_secs(90))
        .user_agent(concat!("llm-proxy/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the upstream HTTP client")?;

    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .with_context(|| {
            format!(
                "invalid bind address {}:{}",
                config.server.host, config.server.port
            )
        })?;
    let authenticated = !config.server.api_keys.is_empty();

    let state = AppState {
        config: Arc::new(config),
        http,
    };

    let app = Router::new()
        .route("/health", get(proxy::health))
        .route("/v1/models", get(proxy::list_models))
        .route("/models", get(proxy::list_models))
        .fallback(proxy::proxy)
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    info!(
        "listening on http://{addr}/v1 (client auth: {})",
        if authenticated { "required" } else { "open" }
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("running the server")?;
    Ok(())
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_ok() {
        info!("shutting down");
    }
}
