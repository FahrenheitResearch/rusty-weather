use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, Subcommand};
use rustwx_core::ModelId;
use rw_ingest::{IngestSupportStatus, model_ingest_capability};
use rw_query::{QueryLimits, StoreCatalog};
use rw_server::config::LogFormat;
use rw_server::{AppConfig, AppState, TokenSet, build_router};
use serde::Serialize;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

type AnyError = Box<dyn Error + Send + Sync>;

#[derive(Debug, Parser)]
#[command(
    name = "rw-server",
    version,
    about = "Self-hosted Rusty Weather data and analytics service"
)]
struct Cli {
    /// TOML configuration file. Environment overrides are applied afterwards.
    #[arg(long, global = true, env = "RW_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the HTTP service. This is the default command.
    Serve,
    /// Validate configuration, credentials, directories, and the store catalog.
    Doctor,
    /// Print the effective configuration after environment overrides.
    PrintConfig,
    /// Print the JSON Schema for the versioned service configuration.
    ConfigSchema,
    /// Print the generated OpenAPI v1 document.
    Openapi,
    /// Print built-in and stored model capabilities as JSON.
    Models,
    /// Probe the configured readiness endpoint without requiring authentication.
    Healthcheck {
        /// Connection timeout in seconds.
        #[arg(long, default_value_t = 5)]
        timeout_seconds: u64,
    },
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: &'static str,
    ok: bool,
    detail: String,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    ok: bool,
    checks: Vec<DoctorCheck>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("rw-server: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), AnyError> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(cli.config.as_deref()).await,
        Command::Doctor => doctor(cli.config.as_deref()),
        Command::PrintConfig => print_config(cli.config.as_deref()),
        Command::ConfigSchema => {
            let schema = rw_server::config_schema_document();
            println!("{}", serde_json::to_string_pretty(&schema)?);
            Ok(())
        }
        Command::Openapi => {
            println!(
                "{}",
                serde_json::to_string_pretty(&rw_server::openapi::document())?
            );
            Ok(())
        }
        Command::Models => print_models(cli.config.as_deref()),
        Command::Healthcheck { timeout_seconds } => {
            healthcheck(cli.config.as_deref(), Duration::from_secs(timeout_seconds))
        }
    }
}

async fn serve(config_path: Option<&Path>) -> Result<(), AnyError> {
    let config = AppConfig::load(config_path)?;
    let tokens = TokenSet::load(&config.auth)?;
    config.validate(!tokens.is_empty())?;
    initialize_tracing(&config)?;

    fs::create_dir_all(&config.server.store_root)?;
    fs::create_dir_all(&config.server.artifact_root)?;
    ensure_real_directory(&config.server.store_root, "store_root")?;
    ensure_real_directory(&config.server.artifact_root, "artifact_root")?;
    ensure_distinct_roots(&config.server.store_root, &config.server.artifact_root)?;

    if tokens.is_empty() {
        warn!(
            listen = %config.server.listen,
            "authentication is disabled; safe configuration restricts this instance to loopback"
        );
    }
    let listen = config.server.listen;
    let state = AppState::new(config, tokens)?;
    let router = build_router(state)?;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!(address = %listener.local_addr()?, "Rusty Weather service listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    info!("Rusty Weather service stopped");
    Ok(())
}

fn doctor(config_path: Option<&Path>) -> Result<(), AnyError> {
    let mut checks = Vec::new();
    let config = match AppConfig::load(config_path) {
        Ok(config) => {
            checks.push(ok("configuration", "configuration parsed"));
            config
        }
        Err(error) => {
            checks.push(fail("configuration", error.to_string()));
            print_doctor(checks)?;
            return Err(error.into());
        }
    };
    let tokens = match TokenSet::load(&config.auth) {
        Ok(tokens) => {
            checks.push(ok(
                "authentication",
                format!("{} API token(s) loaded", tokens.len()),
            ));
            tokens
        }
        Err(error) => {
            checks.push(fail("authentication", error.to_string()));
            print_doctor(checks)?;
            return Err(error.into());
        }
    };
    match config.validate(!tokens.is_empty()) {
        Ok(()) => checks.push(ok("safety_policy", "bind and resource limits are valid")),
        Err(error) => checks.push(fail("safety_policy", error.to_string())),
    }
    checks.push(check_directory("store_root", &config.server.store_root));
    checks.push(check_directory(
        "artifact_root",
        &config.server.artifact_root,
    ));
    match ensure_distinct_roots(&config.server.store_root, &config.server.artifact_root) {
        Ok(()) => checks.push(ok(
            "root_isolation",
            "store and artifact roots are distinct",
        )),
        Err(error) => checks.push(fail("root_isolation", error.to_string())),
    }

    if config.server.store_root.is_dir() {
        let catalog = StoreCatalog::with_limits(
            &config.server.store_root,
            QueryLimits {
                max_catalog_entries: 10_000,
                max_time_points: config.limits.catalog_time_points,
                max_selected_time_points: config.limits.temporal_frames,
                max_variables: config.limits.variables_per_query,
                max_reduction_cells: config.limits.sync_result_values,
                max_temporal_reduction_cells: config.limits.temporal_reduction_cells,
                max_temporal_output_values: config.limits.temporal_output_values,
                max_point_values: config.limits.sync_result_values,
            },
        );
        match catalog.list_models() {
            Ok(models) => checks.push(ok(
                "catalog",
                format!("{} stored model namespace(s) validated", models.len()),
            )),
            Err(error) => checks.push(fail("catalog", error.to_string())),
        }
    }

    let all_ok = checks.iter().all(|check| check.ok);
    print_doctor(checks)?;
    if all_ok {
        Ok(())
    } else {
        Err(std::io::Error::other("one or more doctor checks failed").into())
    }
}

fn print_doctor(checks: Vec<DoctorCheck>) -> Result<(), AnyError> {
    let report = DoctorReport {
        ok: checks.iter().all(|check| check.ok),
        checks,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn print_config(config_path: Option<&Path>) -> Result<(), AnyError> {
    let config = AppConfig::load(config_path)?;
    let tokens = TokenSet::load(&config.auth)?;
    config.validate(!tokens.is_empty())?;
    println!("{}", toml::to_string_pretty(&config)?);
    Ok(())
}

fn print_models(config_path: Option<&Path>) -> Result<(), AnyError> {
    let config = AppConfig::load(config_path)?;
    let mut stored = BTreeMap::new();
    if config.server.store_root.is_dir() {
        for entry in StoreCatalog::new(&config.server.store_root).list_models()? {
            stored.insert(entry.model, entry.run_count);
        }
    }
    let models: Vec<_> = rustwx_models::built_in_models()
        .iter()
        .filter(|summary| summary.id != ModelId::RrfsFireWx)
        .map(|summary| {
            let capability = model_ingest_capability(summary.id);
            let status = match (summary.id, capability.status) {
                (ModelId::WrfGdex, _) => "local_import",
                (_, IngestSupportStatus::Ready) => "ready",
                (_, IngestSupportStatus::Unsupported) => "unsupported",
            };
            serde_json::json!({
                "id": summary.id.to_string(),
                "description": summary.description,
                "cycle_hours_utc": summary.cycle_hours_utc,
                "max_forecast_hour": summary.max_forecast_hour,
                "source_count": summary.sources.len(),
                "ingest_status": status,
                "limitations": capability.limitations.iter().map(|item| item.as_str()).collect::<Vec<_>>(),
                "products": capability.products.iter().map(|product| serde_json::json!({
                    "name": product.product,
                    "surface_source": product.surface_source,
                    "pressure_source": product.pressure_source,
                    "indexed_subset": !product.idx_patterns.is_empty(),
                })).collect::<Vec<_>>(),
                "stored_runs": stored.get(&summary.id.to_string()).copied().unwrap_or(0),
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&models)?);
    Ok(())
}

fn healthcheck(config_path: Option<&Path>, timeout: Duration) -> Result<(), AnyError> {
    let config = AppConfig::load(config_path)?;
    let address = connectable_address(config.server.listen);
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "GET /v1/health/ready HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    stream.take(64 * 1024).read_to_end(&mut response)?;
    let first_line = response
        .split(|byte| *byte == b'\n')
        .next()
        .map(|line| String::from_utf8_lossy(line).trim().to_string())
        .unwrap_or_default();
    if first_line.starts_with("HTTP/1.1 200") || first_line.starts_with("HTTP/1.0 200") {
        println!("ready: {address}");
        Ok(())
    } else {
        Err(std::io::Error::other(format!("readiness probe failed: {first_line}")).into())
    }
}

fn initialize_tracing(config: &AppConfig) -> Result<(), AnyError> {
    let filter = EnvFilter::try_new(&config.logging.filter)?;
    match config.logging.format {
        LogFormat::Json => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .try_init()?,
        LogFormat::Pretty => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .pretty()
            .try_init()?,
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            warn!(%error, "failed to install Ctrl-C handler");
        }
    };

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let terminate = async {
            match signal(SignalKind::terminate()) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(error) => warn!(%error, "failed to install SIGTERM handler"),
            }
        };
        tokio::select! {
            () = ctrl_c => {},
            () = terminate => {},
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;
}

fn connectable_address(address: SocketAddr) -> SocketAddr {
    match address.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), address.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), address.port())
        }
        _ => address,
    }
}

fn ensure_distinct_roots(store_root: &Path, artifact_root: &Path) -> Result<(), AnyError> {
    let store = fs::canonicalize(store_root)?;
    let artifact = fs::canonicalize(artifact_root)?;
    if store == artifact || store.starts_with(&artifact) || artifact.starts_with(&store) {
        return Err(std::io::Error::other(
            "store_root and artifact_root must be separate, non-nested directories",
        )
        .into());
    }
    Ok(())
}

fn ensure_real_directory(path: &Path, label: &str) -> Result<(), AnyError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(std::io::Error::other(format!(
            "{label} must be a real directory, not a symlink"
        ))
        .into());
    }
    Ok(())
}

fn check_directory(name: &'static str, path: &Path) -> DoctorCheck {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            match fs::read_dir(path) {
                Ok(_) => ok(name, format!("{} is a readable directory", path.display())),
                Err(error) => fail(name, error.to_string()),
            }
        }
        Ok(_) => fail(name, format!("{} is not a real directory", path.display())),
        Err(error) => fail(name, error.to_string()),
    }
}

fn ok(name: &'static str, detail: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name,
        ok: true,
        detail: detail.into(),
    }
}

fn fail(name: &'static str, detail: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name,
        ok: false,
        detail: detail.into(),
    }
}
