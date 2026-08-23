mod cast;
mod config;
mod media;
mod payload;
mod web;

use clap::Parser;
use config::{Cli, Config};

#[tokio::main]
async fn main() {
    // Default to `info` when RUST_LOG is unset/empty, so the server always logs
    // its request flow without requiring the caller to set the env var.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let cli = Cli::parse();
    let cfg = match config::build(cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(1);
        }
    };
    log_startup(&cfg);

    let addr = cfg.listen;
    let app = web::build(cfg);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tracing::info!("listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}

/// Log the resolved configuration once at startup so the operator can see
/// exactly what the server will do before any request arrives.
fn log_startup(cfg: &Config) {
    tracing::info!(
        listen = %cfg.listen,
        advertise_url = cfg.advertise_url.as_ref().map(|u| u.as_str()).unwrap_or(""),
        doorbell_url = cfg.doorbell_url.as_deref().unwrap_or(""),
        doorbell_file_loaded = cfg.doorbell_bytes.is_some(),
        livestream_url = cfg.livestream_url.as_ref().map(|u| u.as_str()).unwrap_or(""),
        "startup config"
    );
    if let Some(adv) = &cfg.advertise_url {
        if config::advertise_is_local(adv) {
            tracing::warn!(
                advertise_url = %adv,
                "advertise-url points at localhost; Chromecast devices cannot reach \
                 127.0.0.1/::1/localhost. Set --advertise-url to this server's LAN IP \
                 (e.g. http://192.168.1.10:8080) or media fetches will silently fail."
            );
        }
    }
    for (alias, ip) in &cfg.devices {
        let actions = cfg.actions.get(alias).cloned().unwrap_or_default();
        tracing::info!(alias, ip, actions = ?actions, "device");
    }
    for (path, aliases) in &cfg.routes {
        tracing::info!(path, devices = ?aliases, "route");
    }
}
