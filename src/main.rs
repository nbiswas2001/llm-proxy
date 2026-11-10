mod balance;
mod config;
mod proxy;
mod transcript;

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{Router, routing::get};
use clap::Parser;
use tower_http::cors::CorsLayer;
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::transcript::Transcript;

/// OpenAI-compatible proxy in front of an online LLM API.
#[derive(Parser, Debug)]
#[command(name = "llm-proxy", version, about, long_about = None)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(short, long, default_value = "config.toml", env = "LLM_PROXY_CONFIG")]
    config: PathBuf,
    /// Provider to use for this run, by its [providers.<name>] section name.
    /// Without it, the provider marked `default = true` serves the run.
    #[arg(long, env = "LLM_PROXY_PROVIDER")]
    provider: Option<String>,
    /// Override the port from the config file.
    #[arg(short, long)]
    port: Option<u16>,
    /// Override the bind address from the config file.
    #[arg(long)]
    host: Option<String>,
    /// Path to a .env file holding API keys. Defaults to `.env` beside the config
    /// file, then the nearest `.env` from the working directory.
    #[arg(long)]
    env_file: Option<PathBuf>,
    /// Log each request on screen: provider, model, status, timing. Repeat
    /// (-vv) to also log request bodies and individual response chunks.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
    /// Validate the config and exit without binding a port.
    #[arg(long)]
    check: bool,
    /// Print the credit left on the active provider and exit. Only providers
    /// that publish a balance endpoint can answer this.
    #[arg(long)]
    balance: bool,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    pub transcript: Transcript,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Before logging is set up, so LLM_PROXY_LOG can also come from the .env file.
    let env_file = load_env_file(cli.env_file.as_deref(), &cli.config)?;

    let logger = tracing_subscriber::fmt()
        .with_target(false)
        // An explicit LLM_PROXY_LOG wins; otherwise -v picks the level.
        .with_env_filter(
            EnvFilter::try_from_env("LLM_PROXY_LOG")
                .unwrap_or_else(|_| EnvFilter::new(log_filter(cli.verbose, cli.balance))),
        );
    if cli.balance {
        // --balance owns stdout: the report is the only thing written there, so
        // anything -v or LLM_PROXY_LOG turns on goes to stderr instead.
        logger.with_writer(std::io::stderr).init();
    } else {
        logger.init();
    }

    match &env_file {
        Some(path) => info!("loaded secrets from {}", path.display()),
        None => debug!("no .env file found; using the environment as-is"),
    }

    if !cli.config.exists() {
        anyhow::bail!(
            "config file {} not found - copy config.example.toml to it and add your API key",
            cli.config.display()
        );
    }
    let mut config = Config::load(&cli.config, cli.provider.as_deref())?;
    if let Some(port) = cli.port {
        config.server.port = port;
    }
    if let Some(host) = cli.host {
        config.server.host = host;
    }

    let chosen_by = if cli.provider.is_some() {
        "--provider"
    } else {
        "default"
    };
    let (active_name, _) = config.active();
    for (name, provider) in &config.providers {
        if name == active_name {
            info!(
                "provider \"{name}\" ACTIVE ({chosen_by}) -> {} [api key: {}]",
                provider.base_url,
                if provider.api_key.is_some() {
                    "yes"
                } else {
                    "none"
                }
            );
        } else {
            info!("provider \"{name}\" available");
        }
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

    // Like --check, a query-and-exit mode: nothing is bound, nothing is served.
    if cli.balance {
        return balance::report(&config, &http).await;
    }

    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .with_context(|| {
            format!(
                "invalid bind address {}:{}",
                config.server.host, config.server.port
            )
        })?;
    let authenticated = !config.server.api_keys.is_empty();

    let transcript = if config.server.log_dir.is_empty() {
        info!("transcript off (log_dir is empty)");
        Transcript::disabled()
    } else {
        // Like .env, a relative log folder sits beside the config file.
        let dir = cli
            .config
            .parent()
            .unwrap_or(Path::new("."))
            .join(&config.server.log_dir);
        info!("writing requests and responses to {}", dir.display());
        Transcript::start(dir.clone())
            .with_context(|| format!("creating log folder {}", dir.display()))?
    };

    let state = AppState {
        config: Arc::new(config),
        http,
        transcript,
    };

    let app = Router::new()
        .route("/health", get(proxy::health))
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

/// Verbosity from `-v` flags: quiet startup-only by default, per-request lines
/// at `-v`, request bodies and response chunks at `-vv`. `--balance` prints a
/// report rather than serving, so it stays silent until a `-v` asks otherwise.
fn log_filter(verbose: u8, balance: bool) -> &'static str {
    match verbose {
        0 if balance => "off",
        0 => "llm_proxy=info,tower_http=warn",
        1 => "llm_proxy=debug,tower_http=info",
        _ => "llm_proxy=trace,tower_http=debug",
    }
}

/// Load API keys from a .env file so config placeholders like "${DEEPSEEK_API_KEY}"
/// resolve. Real environment variables take precedence over file entries.
fn load_env_file(explicit: Option<&Path>, config_path: &Path) -> Result<Option<PathBuf>> {
    if let Some(path) = explicit {
        dotenvy::from_path(path).with_context(|| format!("reading env file {}", path.display()))?;
        return Ok(Some(path.to_path_buf()));
    }
    // Keys usually sit next to the config file they are referenced from.
    let beside_config = config_path.parent().unwrap_or(Path::new(".")).join(".env");
    if beside_config.is_file() {
        dotenvy::from_path(&beside_config)
            .with_context(|| format!("reading env file {}", beside_config.display()))?;
        return Ok(Some(beside_config));
    }
    Ok(dotenvy::dotenv().ok())
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_ok() {
        info!("shutting down");
    }
}
