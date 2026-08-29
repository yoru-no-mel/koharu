//! Headless launch mode: the shared runtime serving the HTTP API with no
//! window. Configuration comes from `headless.json` (exclusive to this mode)
//! with CLI flags taking precedence; the shared `~/.koharu/config.toml` is
//! read but never written.

use std::{net::SocketAddr, path::Path, path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use koharu_app::core::App;
use serde::Deserialize;

/// Default listen port for the headless API.
pub const DEFAULT_PORT: u16 = 9170;

/// `headless.json` — launch-time settings. The `pipeline`, `providers`, and
/// `typesetting` sections deep-merge over the live config values in memory
/// only; persistent edits belong in `~/.koharu/config.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct HeadlessConfig {
    host: Option<String>,
    port: Option<u16>,
    warmup: Option<Warmup>,
    pipeline: Option<serde_json::Value>,
    providers: Option<serde_json::Value>,
    typesetting: Option<serde_json::Value>,
}

/// Model warmup performed before the API reports ready.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Warmup {
    All,
    Stages(Vec<koharu_pipeline::Stage>),
}

pub struct Options {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub config: Option<PathBuf>,
    pub store: Option<PathBuf>,
}

pub async fn run(options: Options) -> Result<()> {
    let config_path = match options.config {
        Some(path) => {
            anyhow::ensure!(
                path.exists(),
                "the requested headless config {} does not exist",
                path.display()
            );
            path
        }
        None => default_config_path()?,
    };
    let config = load_config(&config_path)?;

    let store = options.store.unwrap_or_else(default_store_root);
    koharu_runtime::Store::configure(store)
        .context("failed to configure the runtime store")?;

    // Overlays must land before `App::initialize` reads the pipeline config.
    apply_overlays(&config)?;

    let app = Arc::new(App::new()?);

    let host = options
        .host
        .or(config.host)
        .unwrap_or_else(|| "127.0.0.1".to_owned());
    let port = options.port.or(config.port).unwrap_or(DEFAULT_PORT);
    let addr = SocketAddr::new(
        host.parse().with_context(|| format!("invalid host {host:?}"))?,
        port,
    );
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    tracing::info!("headless API listening on http://{addr}");
    println!("Koharu headless listening on http://{addr}");
    println!("OpenAPI: http://{addr}/openapi.json");
    println!("Events:  http://{addr}/api/v1/events");

    let serve_app = app.clone();
    let server = tokio::spawn(async move {
        koharu_rpc::serve(serve_app, listener)
            .await
            .context("the headless server stopped unexpectedly")
    });

    // The API answers 503 until initialization and warmup finish and the app
    // is marked ready, so models may download before the first request lands.
    app.initialize().await?;
    if let Some(warmup) = &config.warmup {
        let stages = match warmup {
            Warmup::All => koharu_pipeline::Stage::ALL.to_vec(),
            Warmup::Stages(stages) => stages.clone(),
        };
        app.warm(stages).await?;
    }
    app.mark_ready();

    server.await??;
    Ok(())
}

fn default_config_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("could not determine the home directory")?
        .join(".koharu")
        .join("headless.json"))
}

/// The runtime store lives next to the executable in portable headless
/// deployments; `--store` relocates it.
fn default_store_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir)
        .join("store")
}

fn load_config(path: &PathBuf) -> Result<HeadlessConfig> {
    if !path.exists() {
        tracing::debug!(
            path = %path.display(),
            "no headless config found; using defaults"
        );
        return Ok(HeadlessConfig::default());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))
}

/// Deep-merges the configured sections over the shared config handles
/// in memory. `Config::save` is intentionally not called: headless.json is
/// an overlay, and `~/.koharu/config.toml` stays untouched.
fn apply_overlays(config: &HeadlessConfig) -> Result<()> {
    if let Some(pipeline) = &config.pipeline {
        overlay(koharu_pipeline::PipelineConfig::load()?, pipeline)?;
    }
    if let Some(providers) = &config.providers {
        overlay(koharu_translator::ProvidersConfig::load()?, providers)?;
    }
    if let Some(typesetting) = &config.typesetting {
        overlay(koharu_renderer::TypesettingConfig::load()?, typesetting)?;
    }
    Ok(())
}

fn overlay<T>(section: koharu_config::Config<T>, update: &serde_json::Value) -> Result<()>
where
    T: Default
        + serde::Serialize
        + serde::de::DeserializeOwned
        + Send
        + Sync
        + 'static,
{
    let mut merged = serde_json::to_value(&*section.read()?)
        .context("failed to serialize the current configuration")?;
    merge_json(&mut merged, update.clone());
    *section.write()? = serde_json::from_value(merged)
        .context("failed to apply the headless overlay")?;
    Ok(())
}

/// Structural deep merge: objects merge recursively, every other value
/// (including arrays) replaces the base wholesale.
fn merge_json(base: &mut serde_json::Value, update: serde_json::Value) {
    match (base, update) {
        (serde_json::Value::Object(base), serde_json::Value::Object(update)) => {
            for (key, value) in update {
                match base.get_mut(&key) {
                    Some(base) => merge_json(base, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, update) => *base = update,
    }
}
