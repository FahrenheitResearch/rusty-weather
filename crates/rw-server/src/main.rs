use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, Subcommand};
use rustwx_core::ModelId;
use rw_ingest::{IngestSupportStatus, indexed_subset_available, model_ingest_capability};
use rw_ops_protocol::StormModelManifest;
use rw_query::{QueryLimits, StoreCatalog};
use rw_server::config::LogFormat;
use rw_server::generation_replication::ServerGenerationReplication;
use rw_server::mrms_ingest::MrmsIngestSupervisor;
use rw_server::nexrad_level2_ingest::NexradLevel2IngestSupervisor;
use rw_server::origin_catalog::PublishedStoreCatalog;
use rw_server::satellite_ingest::SatelliteIngestSupervisor;
use rw_server::satellite_prewarm::SatellitePrewarmSupervisor;
use rw_server::storm_prewarm::StormPrewarmSupervisor;
use rw_server::{AppConfig, AppState, TokenSet, build_router};
use rw_storm_ml::{ModelKey, ModelLimits, ModelRegistry, ModelUsePolicy};
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
    /// Install and atomically select private storm-segmentation model versions.
    StormModels {
        #[command(subcommand)]
        command: StormModelCommand,
    },
    /// Probe the configured readiness endpoint without requiring authentication.
    Healthcheck {
        /// Connection timeout in seconds.
        #[arg(long, default_value_t = 5)]
        timeout_seconds: u64,
    },
}

#[derive(Debug, Subcommand)]
enum StormModelCommand {
    /// List installed versions, enablement, active selection, and rights metadata.
    List,
    /// Install one immutable version from local manifest, policy, and artifact files.
    Install {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        policy: PathBuf,
        #[arg(long)]
        artifact: PathBuf,
    },
    /// Verify every installed artifact, or one exact model version, against its digest.
    Verify {
        #[arg(long, requires = "model_version")]
        model_id: Option<String>,
        #[arg(long, requires = "model_id")]
        model_version: Option<String>,
    },
    /// Permit an installed version to execute. This does not make it active.
    Enable {
        #[arg(long)]
        model_id: String,
        #[arg(long)]
        model_version: String,
    },
    /// Prevent an installed version from executing; an active version is deselected.
    Disable {
        #[arg(long)]
        model_id: String,
        #[arg(long)]
        model_version: String,
    },
    /// Atomically make an enabled version active for its model ID.
    Activate {
        #[arg(long)]
        model_id: String,
        #[arg(long)]
        model_version: String,
    },
    /// Atomically return a model ID to its most recently active enabled version.
    Rollback {
        #[arg(long)]
        model_id: String,
    },
}

#[derive(Debug, Serialize)]
struct StormModelRecord<'a> {
    model_id: &'a str,
    model_version: &'a str,
    enabled: bool,
    active: bool,
    backend: rw_ops_protocol::StormModelBackend,
    artifact_sha256: &'a str,
    display_name: &'a str,
    producer: &'a str,
    license: Option<&'a str>,
    training_provenance: Option<&'a str>,
    artifact_distribution: rw_storm_ml::DistributionGrant,
    derived_output_distribution: rw_storm_ml::DistributionGrant,
    required_attribution: &'a str,
    rights_reference: &'a str,
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
        Command::StormModels { command } => manage_storm_models(cli.config.as_deref(), command),
        Command::Healthcheck { timeout_seconds } => {
            healthcheck(cli.config.as_deref(), Duration::from_secs(timeout_seconds))
        }
    }
}

fn manage_storm_models(
    config_path: Option<&Path>,
    command: StormModelCommand,
) -> Result<(), AnyError> {
    let config = AppConfig::load(config_path)?;
    if !config.operations.enabled {
        return Err(std::io::Error::other(
            "storm-model administration requires operations.enabled=true so the registry root is explicit",
        )
        .into());
    }
    let root = std::path::absolute(&config.operations.root)?.join("storm-models");
    let limits = ModelLimits::default();
    let mut registry = ModelRegistry::open(&root, limits)?;

    let output = match command {
        StormModelCommand::List => serde_json::json!({
            "schema": "rw.server.storm-model-admin.v1",
            "action": "list",
            "registry_root": root,
            "models": storm_model_records(&registry),
        }),
        StormModelCommand::Install {
            manifest,
            policy,
            artifact,
        } => {
            let manifest: StormModelManifest =
                read_bounded_json(&manifest, limits.max_manifest_bytes, "model manifest")?;
            let policy: ModelUsePolicy =
                read_bounded_json(&policy, limits.max_manifest_bytes, "model policy")?;
            let artifact_file = fs::File::open(&artifact)?;
            let installed = registry.install(manifest, policy, artifact_file)?;
            serde_json::json!({
                "schema": "rw.server.storm-model-admin.v1",
                "action": "install",
                "model_id": installed.key.model_id,
                "model_version": installed.key.model_version,
                "artifact_sha256": installed.manifest.artifact_sha256,
                "enabled": false,
                "active": false,
                "restart_required_for_running_server": true,
            })
        }
        StormModelCommand::Verify {
            model_id,
            model_version,
        } => {
            let keys = if let (Some(model_id), Some(model_version)) = (model_id, model_version) {
                vec![ModelKey::new(model_id, model_version)?]
            } else {
                registry
                    .installed()
                    .map(|model| model.key.clone())
                    .collect()
            };
            for key in &keys {
                registry.get(key)?.open_verified_artifact(limits)?;
            }
            serde_json::json!({
                "schema": "rw.server.storm-model-admin.v1",
                "action": "verify",
                "verified": keys,
            })
        }
        StormModelCommand::Enable {
            model_id,
            model_version,
        } => {
            let key = ModelKey::new(model_id, model_version)?;
            registry.enable(&key)?;
            storm_model_action("enable", &key)
        }
        StormModelCommand::Disable {
            model_id,
            model_version,
        } => {
            let key = ModelKey::new(model_id, model_version)?;
            registry.disable(&key)?;
            storm_model_action("disable", &key)
        }
        StormModelCommand::Activate {
            model_id,
            model_version,
        } => {
            let key = ModelKey::new(model_id, model_version)?;
            registry.activate(&key)?;
            storm_model_action("activate", &key)
        }
        StormModelCommand::Rollback { model_id } => {
            let selected = registry.rollback(&model_id)?;
            storm_model_action("rollback", &selected.key)
        }
    };

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn storm_model_action(action: &'static str, key: &ModelKey) -> serde_json::Value {
    serde_json::json!({
        "schema": "rw.server.storm-model-admin.v1",
        "action": action,
        "model_id": key.model_id,
        "model_version": key.model_version,
        "restart_required_for_running_server": true,
    })
}

fn storm_model_records(registry: &ModelRegistry) -> Vec<StormModelRecord<'_>> {
    registry
        .installed()
        .map(|model| StormModelRecord {
            model_id: &model.key.model_id,
            model_version: &model.key.model_version,
            enabled: registry.is_enabled(&model.key),
            active: registry
                .active(&model.key.model_id)
                .is_ok_and(|active| active.key == model.key),
            backend: model.manifest.backend,
            artifact_sha256: &model.manifest.artifact_sha256,
            display_name: &model.manifest.display_name,
            producer: &model.manifest.producer,
            license: model.manifest.license.as_deref(),
            training_provenance: model.manifest.training_provenance.as_deref(),
            artifact_distribution: model.policy.artifact_distribution,
            derived_output_distribution: model.policy.derived_output_distribution,
            required_attribution: &model.policy.required_attribution,
            rights_reference: &model.policy.rights_reference,
        })
        .collect()
}

fn read_bounded_json<T: serde::de::DeserializeOwned>(
    path: &Path,
    maximum_bytes: u64,
    resource: &'static str,
) -> Result<T, AnyError> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(std::io::Error::other(format!(
            "{resource} is not a regular file: {}",
            path.display()
        ))
        .into());
    }
    if metadata.len() > maximum_bytes {
        return Err(std::io::Error::other(format!(
            "{resource} is {} bytes; configured maximum is {maximum_bytes}",
            metadata.len()
        ))
        .into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    fs::File::open(path)?
        .take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum_bytes {
        return Err(std::io::Error::other(format!(
            "{resource} grew beyond configured maximum {maximum_bytes} while being read"
        ))
        .into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

async fn serve(config_path: Option<&Path>) -> Result<(), AnyError> {
    let config = AppConfig::load(config_path)?;
    let tokens = TokenSet::load(&config.auth)?;
    config.validate(!tokens.is_empty())?;
    if config.operations.enabled {
        tokens.operations_credential_count(&config.auth)?;
    }
    initialize_tracing(&config)?;

    fs::create_dir_all(&config.server.store_root)?;
    fs::create_dir_all(&config.server.artifact_root)?;
    fs::create_dir_all(&config.server.cache_root)?;
    ensure_real_directory(&config.server.store_root, "store_root")?;
    ensure_real_directory(&config.server.artifact_root, "artifact_root")?;
    ensure_real_directory(&config.server.cache_root, "cache_root")?;
    ensure_distinct_roots(&config.server.store_root, &config.server.artifact_root)?;
    ensure_distinct_roots(&config.server.store_root, &config.server.cache_root)?;
    ensure_distinct_roots(&config.server.artifact_root, &config.server.cache_root)?;
    if config.mrms_ingest.enabled {
        probe_writable_directory(&config.server.store_root)?;
    }
    if config.nexrad_level2_ingest.enabled {
        probe_writable_directory(&config.server.store_root)?;
        fs::create_dir_all(&config.nexrad_level2_ingest.state_root)?;
        ensure_real_directory(
            &config.nexrad_level2_ingest.state_root,
            "nexrad_level2_ingest.state_root",
        )?;
        for root in [
            &config.server.store_root,
            &config.server.artifact_root,
            &config.server.cache_root,
        ] {
            ensure_distinct_roots(root, &config.nexrad_level2_ingest.state_root)?;
        }
    }
    if config.satellite_ingest.enabled {
        fs::create_dir_all(&config.satellite_ingest.raw_cache_root)?;
        ensure_real_directory(
            &config.satellite_ingest.raw_cache_root,
            "satellite_ingest.raw_cache_root",
        )?;
        for root in [
            &config.server.store_root,
            &config.server.artifact_root,
            &config.server.cache_root,
        ] {
            ensure_distinct_roots(root, &config.satellite_ingest.raw_cache_root)?;
        }
        if config.nexrad_level2_ingest.enabled {
            ensure_distinct_roots(
                &config.nexrad_level2_ingest.state_root,
                &config.satellite_ingest.raw_cache_root,
            )?;
        }
    }
    if config.community.enabled {
        fs::create_dir_all(&config.community.root)?;
        ensure_real_directory(&config.community.root, "community.root")?;
        ensure_distinct_roots(&config.server.store_root, &config.community.root)?;
        ensure_distinct_roots(&config.server.artifact_root, &config.community.root)?;
        ensure_distinct_roots(&config.server.cache_root, &config.community.root)?;
        if config.satellite_ingest.enabled {
            ensure_distinct_roots(
                &config.satellite_ingest.raw_cache_root,
                &config.community.root,
            )?;
        }
        if config.nexrad_level2_ingest.enabled {
            ensure_distinct_roots(
                &config.nexrad_level2_ingest.state_root,
                &config.community.root,
            )?;
        }
    }
    if config.generation_replication.enabled {
        fs::create_dir_all(&config.generation_replication.control_root)?;
        ensure_real_directory(
            &config.generation_replication.control_root,
            "generation_replication.control_root",
        )?;
        for root in [
            &config.server.store_root,
            &config.server.artifact_root,
            &config.server.cache_root,
        ] {
            ensure_distinct_roots(root, &config.generation_replication.control_root)?;
        }
        if config.community.enabled {
            ensure_distinct_roots(
                &config.community.root,
                &config.generation_replication.control_root,
            )?;
        }
        if config.satellite_ingest.enabled {
            ensure_distinct_roots(
                &config.satellite_ingest.raw_cache_root,
                &config.generation_replication.control_root,
            )?;
        }
        if config.nexrad_level2_ingest.enabled {
            ensure_distinct_roots(
                &config.nexrad_level2_ingest.state_root,
                &config.generation_replication.control_root,
            )?;
        }
    }
    if config.operations.enabled {
        fs::create_dir_all(&config.operations.root)?;
        ensure_real_directory(&config.operations.root, "operations.root")?;
        ensure_distinct_roots(&config.server.store_root, &config.operations.root)?;
        ensure_distinct_roots(&config.server.artifact_root, &config.operations.root)?;
        ensure_distinct_roots(&config.server.cache_root, &config.operations.root)?;
        if config.satellite_ingest.enabled {
            ensure_distinct_roots(
                &config.satellite_ingest.raw_cache_root,
                &config.operations.root,
            )?;
        }
        if config.community.enabled {
            ensure_distinct_roots(&config.community.root, &config.operations.root)?;
        }
        if config.generation_replication.enabled {
            ensure_distinct_roots(
                &config.generation_replication.control_root,
                &config.operations.root,
            )?;
        }
        if config.nexrad_level2_ingest.enabled {
            ensure_distinct_roots(
                &config.nexrad_level2_ingest.state_root,
                &config.operations.root,
            )?;
        }
    }

    if tokens.is_empty() {
        warn!(
            listen = %config.server.listen,
            "authentication is disabled; safe configuration restricts this instance to loopback"
        );
    }
    let listen = config.server.listen;
    let satellite_ingest_config = config.satellite_ingest.clone();
    let satellite_prewarm_config = config.satellite_prewarm.clone();
    let mrms_ingest_config = config.mrms_ingest.clone();
    let nexrad_level2_ingest_config = config.nexrad_level2_ingest.clone();
    let storm_prewarm_config = config.storm_prewarm.clone();
    let store_root = config.server.store_root.clone();
    let state = AppState::new(config, tokens)?;
    let router = build_router(state.clone())?;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let mut satellite_ingest =
        SatelliteIngestSupervisor::start(&satellite_ingest_config, &store_root)?;
    let mut satellite_prewarm = SatellitePrewarmSupervisor::start(
        satellite_prewarm_config,
        state.clone(),
        satellite_ingest.update_signal(),
    );
    let federation_health_monitor = state.federation.start_health_monitor(state.metrics.clone());
    if satellite_ingest.worker_count() > 0 {
        info!(
            followers = satellite_ingest.worker_count(),
            "server-owned satellite ingest enabled"
        );
    }
    let mut mrms_ingest = MrmsIngestSupervisor::start(
        &mrms_ingest_config,
        &store_root,
        state.mrms_ingest_monitor(),
    );
    let mut nexrad_level2_ingest = NexradLevel2IngestSupervisor::start(
        &nexrad_level2_ingest_config,
        &store_root,
        state.nexrad_level2_ingest_monitor(),
    )
    .map_err(std::io::Error::other)?;
    let mut storm_prewarm = StormPrewarmSupervisor::start(
        storm_prewarm_config,
        state.clone(),
        state.mrms_ingest_monitor().committed_receiver(),
    );
    if mrms_ingest.worker_count() > 0 {
        info!(
            products = mrms_ingest.worker_count(),
            "server-owned MRMS ingest enabled"
        );
    }
    if nexrad_level2_ingest.worker_count() > 0 {
        info!(
            sites = nexrad_level2_ingest.worker_count(),
            "server-owned NEXRAD Level II ingest enabled"
        );
    }
    info!(address = %listener.local_addr()?, "Rusty Weather service listening");
    let serve_result = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    satellite_prewarm.shutdown().await;
    satellite_ingest.shutdown().await;
    mrms_ingest.shutdown().await;
    nexrad_level2_ingest.shutdown().await;
    storm_prewarm.shutdown().await;
    if let Some(task) = federation_health_monitor {
        task.abort();
        let _ = task.await;
    }
    serve_result?;
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
    if config.operations.enabled {
        match tokens.operations_credential_count(&config.auth) {
            Ok(count) => checks.push(ok(
                "operations_authentication",
                format!("{count} disjoint operations credential(s) loaded"),
            )),
            Err(error) => checks.push(fail("operations_authentication", error.to_string())),
        }
    }
    if config.federation.proxy.enabled
        || config.federation.proxy.accept_local_resolve
        || config.federation.health_monitor_enabled
    {
        match rw_server::federation_proxy::validate_credential_isolation(&config, &tokens) {
            Ok(()) => checks.push(ok(
                "federation_credential_isolation",
                "active API, local-resolve, origin data, and health credential domains are value-disjoint",
            )),
            Err(error) => checks.push(fail("federation_credential_isolation", error)),
        }
    }
    checks.push(check_directory("store_root", &config.server.store_root));
    checks.push(check_directory(
        "artifact_root",
        &config.server.artifact_root,
    ));
    checks.push(check_directory("cache_root", &config.server.cache_root));
    match ensure_all_distinct_roots([
        config.server.store_root.as_path(),
        config.server.artifact_root.as_path(),
        config.server.cache_root.as_path(),
    ]) {
        Ok(()) => checks.push(ok(
            "root_isolation",
            "store, artifact, and cache roots are distinct",
        )),
        Err(error) => checks.push(fail("root_isolation", error.to_string())),
    }
    if config.satellite_ingest.enabled {
        checks.push(check_directory(
            "satellite_ingest.raw_cache_root",
            &config.satellite_ingest.raw_cache_root,
        ));
        for (name, root) in [
            ("satellite_store_isolation", &config.server.store_root),
            ("satellite_artifact_isolation", &config.server.artifact_root),
            ("satellite_cache_isolation", &config.server.cache_root),
        ] {
            match ensure_distinct_roots(root, &config.satellite_ingest.raw_cache_root) {
                Ok(()) => checks.push(ok(name, "satellite raw staging root is isolated")),
                Err(error) => checks.push(fail(name, error.to_string())),
            }
        }
        checks.push(ok(
            "satellite_ingest",
            format!(
                "{} server-owned follower(s) configured; NOAA public S3 requires no credentials",
                config.satellite_ingest.followers.len()
            ),
        ));
    }
    if config.mrms_ingest.enabled {
        checks.push(match probe_writable_directory(&config.server.store_root) {
            Ok(()) => ok(
                "mrms_ingest",
                format!(
                    "{} authenticated bounded product follower(s) configured; store is writable",
                    config.mrms_ingest.products.len()
                ),
            ),
            Err(error) => fail("mrms_ingest", error.to_string()),
        });
    }
    if config.nexrad_level2_ingest.enabled {
        checks.push(check_directory(
            "nexrad_level2_ingest.state_root",
            &config.nexrad_level2_ingest.state_root,
        ));
        checks.push(match probe_writable_directory(&config.server.store_root) {
            Ok(()) => ok(
                "nexrad_level2_ingest",
                format!(
                    "{} explicit site follower(s), {} provider adapter(s); exact-time store is writable",
                    config.nexrad_level2_ingest.sites.len(),
                    config.nexrad_level2_ingest.providers.len()
                ),
            ),
            Err(error) => fail("nexrad_level2_ingest", error.to_string()),
        });
        for (name, root) in [
            ("nexrad_state_store_isolation", &config.server.store_root),
            (
                "nexrad_state_artifact_isolation",
                &config.server.artifact_root,
            ),
            ("nexrad_state_cache_isolation", &config.server.cache_root),
        ] {
            match ensure_distinct_roots(root, &config.nexrad_level2_ingest.state_root) {
                Ok(()) => checks.push(ok(name, "NEXRAD cursor state root is isolated")),
                Err(error) => checks.push(fail(name, error.to_string())),
            }
        }
    }
    if config.operations.enabled {
        checks.push(check_directory("operations.root", &config.operations.root));
        for (name, root) in [
            ("operations_store_isolation", &config.server.store_root),
            (
                "operations_artifact_isolation",
                &config.server.artifact_root,
            ),
            ("operations_cache_isolation", &config.server.cache_root),
        ] {
            match ensure_distinct_roots(root, &config.operations.root) {
                Ok(()) => checks.push(ok(name, "operations root is isolated and writable")),
                Err(error) => checks.push(fail(name, error.to_string())),
            }
        }
        if config.nexrad_level2_ingest.enabled {
            match ensure_distinct_roots(
                &config.nexrad_level2_ingest.state_root,
                &config.operations.root,
            ) {
                Ok(()) => checks.push(ok(
                    "nexrad_state_operations_isolation",
                    "NEXRAD cursor state root is isolated",
                )),
                Err(error) => {
                    checks.push(fail("nexrad_state_operations_isolation", error.to_string()))
                }
            }
        }
    }
    if config.community.enabled {
        checks.push(check_directory("community.root", &config.community.root));
        for (name, first, second) in [
            (
                "community_store_isolation",
                config.server.store_root.as_path(),
                config.community.root.as_path(),
            ),
            (
                "community_artifact_isolation",
                config.server.artifact_root.as_path(),
                config.community.root.as_path(),
            ),
            (
                "community_cache_isolation",
                config.server.cache_root.as_path(),
                config.community.root.as_path(),
            ),
        ] {
            match ensure_distinct_roots(first, second) {
                Ok(()) => checks.push(ok(name, "Community Cache root is isolated")),
                Err(error) => checks.push(fail(name, error.to_string())),
            }
        }
        if config.satellite_ingest.enabled {
            match ensure_distinct_roots(
                &config.satellite_ingest.raw_cache_root,
                &config.community.root,
            ) {
                Ok(()) => checks.push(ok(
                    "satellite_community_isolation",
                    "satellite raw staging root is isolated",
                )),
                Err(error) => checks.push(fail("satellite_community_isolation", error.to_string())),
            }
        }
        if config.nexrad_level2_ingest.enabled {
            match ensure_distinct_roots(
                &config.nexrad_level2_ingest.state_root,
                &config.community.root,
            ) {
                Ok(()) => checks.push(ok(
                    "nexrad_state_community_isolation",
                    "NEXRAD cursor state root is isolated",
                )),
                Err(error) => {
                    checks.push(fail("nexrad_state_community_isolation", error.to_string()))
                }
            }
        }
        match rw_server::community::CommunityService::open(&config.community) {
            Ok(_) => checks.push(ok(
                "community_security",
                "signing key, providers, quotas, and cache root validated",
            )),
            Err(error) => checks.push(fail("community_security", error.to_string())),
        }
        if config.community.relay.enabled {
            let limits = rw_community_protocol::ProtocolLimits {
                max_manifest_bytes: config.community.quotas.maximum_manifest_bytes,
                max_encoded_bytes: config.community.quotas.maximum_object_bytes,
                max_decoded_bytes: config.community.quotas.maximum_decompressed_bytes,
                max_case_artifacts: config.community.cases.maximum_objects_per_case,
                ..rw_community_protocol::ProtocolLimits::default()
            };
            match rw_server::community_relay::CommunityRelayService::open(
                &config.community,
                limits,
            ) {
                Ok(_) => checks.push(ok(
                    "community_relay_security",
                    "separate Phase 2 gates, durable state, signing key, provider secret, and audited relay-allocation ranges validated",
                )),
                Err(error) => checks.push(fail("community_relay_security", error.to_string())),
            }
        }
    }
    let mut generation_replication = ServerGenerationReplication::default();
    if config.generation_replication.enabled {
        checks.push(check_directory(
            "generation_replication.control_root",
            &config.generation_replication.control_root,
        ));
        for (name, root) in [
            ("replication_store_isolation", &config.server.store_root),
            (
                "replication_artifact_isolation",
                &config.server.artifact_root,
            ),
            ("replication_cache_isolation", &config.server.cache_root),
        ] {
            match ensure_distinct_roots(root, &config.generation_replication.control_root) {
                Ok(()) => checks.push(ok(name, "replication control root is isolated")),
                Err(error) => checks.push(fail(name, error.to_string())),
            }
        }
        if config.nexrad_level2_ingest.enabled {
            match ensure_distinct_roots(
                &config.nexrad_level2_ingest.state_root,
                &config.generation_replication.control_root,
            ) {
                Ok(()) => checks.push(ok(
                    "nexrad_state_replication_isolation",
                    "NEXRAD cursor state root is isolated",
                )),
                Err(error) => checks.push(fail(
                    "nexrad_state_replication_isolation",
                    error.to_string(),
                )),
            }
        }
        if config.community.enabled {
            match ensure_distinct_roots(
                &config.community.root,
                &config.generation_replication.control_root,
            ) {
                Ok(()) => checks.push(ok(
                    "replication_community_isolation",
                    "replication control root is isolated",
                )),
                Err(error) => {
                    checks.push(fail("replication_community_isolation", error.to_string()))
                }
            }
        }
        if config.satellite_ingest.enabled {
            match ensure_distinct_roots(
                &config.satellite_ingest.raw_cache_root,
                &config.generation_replication.control_root,
            ) {
                Ok(()) => checks.push(ok(
                    "satellite_replication_isolation",
                    "satellite raw staging root is isolated",
                )),
                Err(error) => {
                    checks.push(fail("satellite_replication_isolation", error.to_string()))
                }
            }
        }
        match ServerGenerationReplication::open(
            &config.generation_replication,
            &config.server.store_root,
        ) {
            Ok(service) => match service.startup_status() {
                Ok(status) => {
                    checks.push(ok(
                        "generation_replication",
                        format!(
                            "durable state authenticated; {} active upload(s), {} published generation(s), kill switch {}",
                            status.active_uploads,
                            status.published_generations,
                            if status.kill_switch { "engaged" } else { "disengaged" }
                        ),
                    ));
                    generation_replication = service;
                }
                Err(error) => checks.push(fail("generation_replication", error.to_string())),
            },
            Err(error) => checks.push(fail("generation_replication", error.to_string())),
        }
    }
    if config.federation.enabled {
        match rw_server::federation::FederationService::open(&config.federation) {
            Ok(service) => match service.health_status() {
                Ok(status) => checks.push(ok(
                    "federation_security",
                    format!(
                        "{} signed public origin(s) validated; active health monitor {}",
                        status.total_origins,
                        if status.monitor_enabled {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    ),
                )),
                Err(error) => checks.push(fail("federation_security", error.to_string())),
            },
            Err(error) => checks.push(fail("federation_security", error.to_string())),
        }
    }
    if config.federation.proxy.enabled || config.federation.proxy.accept_local_resolve {
        match rw_server::federation_proxy::doctor_status(&config, &tokens) {
            Ok(status) => checks.push(ok(
                "federation_proxy_security",
                format!(
                    "{} approved origin(s); authority key id {}; durable accounting {}; kill switch {}; local-only resolver {}; dedicated origin credential {}",
                    status.approved_origins,
                    status.authority_signing_key_id,
                    if status.durable_accounting_opened { "opened" } else { "closed" },
                    if status.kill_switch { "engaged" } else { "disengaged" },
                    if status.local_resolve_enabled { "enabled" } else { "disabled" },
                    if status.local_resolve_credential_loaded { "loaded" } else { "not required" },
                ),
            )),
            Err(error) => checks.push(fail("federation_proxy_security", error)),
        }
    }

    if config.server.store_root.is_dir() {
        let catalog = StoreCatalog::with_limits(
            &config.server.store_root,
            QueryLimits {
                // Match the live server: the doctor must validate the entire
                // catalog instead of declaring a large store unhealthy at an
                // arbitrary entry count.
                max_catalog_entries: usize::MAX,
                max_time_points: config.limits.catalog_time_points,
                max_selected_time_points: config.limits.temporal_frames,
                max_variables: config.limits.variables_per_query,
                max_reduction_cells: config.limits.sync_result_values,
                max_temporal_reduction_cells: config.limits.temporal_reduction_cells,
                max_temporal_output_values: config.limits.temporal_output_values,
                max_point_values: config.limits.sync_result_values,
            },
        );
        let catalog = PublishedStoreCatalog::new(catalog, config.origin_catalog.clone())
            .with_generation_replication(generation_replication);
        if config.origin_catalog.enabled {
            let status = catalog.health_status();
            if status.ready {
                checks.push(ok(
                    "origin_catalog",
                    format!(
                        "{} published model namespace(s) and {} generation(s) validated",
                        status.published_models, status.published_runs
                    ),
                ));
            } else {
                checks.push(fail(
                    "origin_catalog",
                    "enabled scheduler publication catalog is not ready",
                ));
            }
        }
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
    let tokens = TokenSet::load(&config.auth)?;
    config.validate(!tokens.is_empty())?;
    let mut stored = BTreeMap::new();
    let restrict_to_published = config.origin_catalog.enabled;
    if config.server.store_root.is_dir() || restrict_to_published {
        let replication = ServerGenerationReplication::open(
            &config.generation_replication,
            &config.server.store_root,
        )?;
        let catalog = PublishedStoreCatalog::new(
            StoreCatalog::new(&config.server.store_root),
            config.origin_catalog.clone(),
        )
        .with_generation_replication(replication);
        for entry in catalog.list_models()? {
            stored.insert(entry.model, entry.run_count);
        }
    }
    let models: Vec<_> = rustwx_models::built_in_models()
        .iter()
        .filter(|summary| summary.id != ModelId::RrfsFireWx)
        .filter(|summary| {
            !restrict_to_published || stored.contains_key(&summary.id.to_string())
        })
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
                    "indexed_subset": indexed_subset_available(summary.id, product),
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

fn ensure_distinct_roots(first_root: &Path, second_root: &Path) -> Result<(), AnyError> {
    let first = fs::canonicalize(first_root)?;
    let second = fs::canonicalize(second_root)?;
    if first == second || first.starts_with(&second) || second.starts_with(&first) {
        return Err(std::io::Error::other(
            "configured roots must be separate, non-nested directories",
        )
        .into());
    }
    Ok(())
}

fn ensure_all_distinct_roots<const N: usize>(roots: [&Path; N]) -> Result<(), AnyError> {
    for first in 0..N {
        for second in (first + 1)..N {
            ensure_distinct_roots(roots[first], roots[second])?;
        }
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

fn probe_writable_directory(path: &Path) -> std::io::Result<()> {
    let probe = path.join(format!(".rw-server-write-probe-{}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    let result = (|| {
        file.write_all(b"rw-server-writable-v1\n")?;
        file.sync_all()
    })();
    drop(file);
    let cleanup = fs::remove_file(&probe);
    result?;
    cleanup
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

#[cfg(test)]
mod storm_model_cli_tests {
    use super::*;
    use rw_ops_protocol::{
        ModelInputSource, STORM_MODEL_MANIFEST_SCHEMA, StormModelBackend, StormModelInput,
    };
    use rw_storm_ml::DistributionGrant;
    use sha2::{Digest, Sha256};

    fn manifest(version: &str, artifact: &[u8]) -> StormModelManifest {
        StormModelManifest {
            schema: STORM_MODEL_MANIFEST_SCHEMA.into(),
            model_id: "example-cell-segmentation".into(),
            model_version: version.into(),
            backend: StormModelBackend::SuppliedMask,
            artifact_sha256: format!("{:x}", Sha256::digest(artifact)),
            display_name: "Example storm segmentation".into(),
            description: "Test-only supplied-mask model for the offline administration CLI.".into(),
            inputs: vec![StormModelInput {
                name: "reflectivity".into(),
                source: ModelInputSource::MrmsProduct,
                field: "mrms_reflectivity_lowest_altitude".into(),
                units: "dBZ".into(),
                minimum: Some(-20.0),
                maximum: Some(80.0),
                missing_value: None,
            }],
            output_name: "storm_probability".into(),
            probability_threshold: 0.5,
            minimum_area_km2: Some(1.0),
            producer: "Fahrenheit Research test suite".into(),
            license: Some("Private test fixture; not for redistribution".into()),
            training_provenance: Some("Synthetic fixture; no learned weights".into()),
        }
    }

    #[test]
    fn clap_exposes_exact_offline_model_lifecycle() {
        for arguments in [
            vec!["rw-server", "storm-models", "list"],
            vec![
                "rw-server",
                "storm-models",
                "enable",
                "--model-id",
                "cell",
                "--model-version",
                "1",
            ],
            vec![
                "rw-server",
                "storm-models",
                "rollback",
                "--model-id",
                "cell",
            ],
        ] {
            Cli::try_parse_from(arguments).unwrap();
        }
    }

    #[test]
    fn offline_cli_installs_verifies_activates_and_rolls_back() {
        let directory = tempfile::tempdir().unwrap();
        let operations_root = directory.path().join("operations");
        let config_path = directory.path().join("rusty-weather.toml");
        let mut config = AppConfig::default();
        config.operations.enabled = true;
        config.operations.root = operations_root.clone();
        fs::write(&config_path, toml::to_string_pretty(&config).unwrap()).unwrap();

        let policy = ModelUsePolicy {
            artifact_distribution: DistributionGrant::NodeOnly,
            derived_output_distribution: DistributionGrant::CompanyInternal,
            required_attribution: "Fahrenheit Research test fixture".into(),
            rights_reference: "example-test-rights-v1".into(),
        };
        let policy_path = directory.path().join("policy.json");
        fs::write(&policy_path, serde_json::to_vec_pretty(&policy).unwrap()).unwrap();

        for (version, artifact) in [
            ("1.0.0", b"fixture-v1".as_slice()),
            ("2.0.0", b"fixture-v2"),
        ] {
            let manifest_path = directory.path().join(format!("manifest-{version}.json"));
            let artifact_path = directory.path().join(format!("artifact-{version}.bin"));
            fs::write(
                &manifest_path,
                serde_json::to_vec_pretty(&manifest(version, artifact)).unwrap(),
            )
            .unwrap();
            fs::write(&artifact_path, artifact).unwrap();
            manage_storm_models(
                Some(&config_path),
                StormModelCommand::Install {
                    manifest: manifest_path,
                    policy: policy_path.clone(),
                    artifact: artifact_path,
                },
            )
            .unwrap();
            manage_storm_models(
                Some(&config_path),
                StormModelCommand::Enable {
                    model_id: "example-cell-segmentation".into(),
                    model_version: version.into(),
                },
            )
            .unwrap();
            manage_storm_models(
                Some(&config_path),
                StormModelCommand::Activate {
                    model_id: "example-cell-segmentation".into(),
                    model_version: version.into(),
                },
            )
            .unwrap();
        }

        manage_storm_models(
            Some(&config_path),
            StormModelCommand::Verify {
                model_id: None,
                model_version: None,
            },
        )
        .unwrap();
        manage_storm_models(
            Some(&config_path),
            StormModelCommand::Rollback {
                model_id: "example-cell-segmentation".into(),
            },
        )
        .unwrap();

        let registry =
            ModelRegistry::open(operations_root.join("storm-models"), ModelLimits::default())
                .unwrap();
        assert_eq!(
            registry
                .active_for_execution("example-cell-segmentation")
                .unwrap()
                .key
                .model_version,
            "1.0.0"
        );
    }
}
