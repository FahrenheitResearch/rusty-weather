use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use clap::{Parser, Subcommand};
use rw_scheduler::config::SchedulerConfig;
use rw_scheduler::{
    ExecutionReport, SchedulerError, SchedulerHost, SchedulerResult, audit_host_capacity,
};

#[derive(Debug, Parser)]
#[command(
    name = "rw-scheduler",
    version,
    about = "Durable rw-store model scheduler"
)]
struct Cli {
    /// TOML configuration file (JSON is accepted when the extension is .json).
    #[arg(long, env = "RW_SCHEDULER_CONFIG")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the newest registry-derived plans without probing providers.
    Plan {
        /// Override the clock for reproducible operations/tests.
        #[arg(long, hide = true)]
        now_unix: Option<i64>,
    },
    /// Probe current provider cycles without downloading model payloads.
    Discover {
        /// Override the clock for reproducible operations/tests.
        #[arg(long, hide = true)]
        now_unix: Option<i64>,
    },
    /// Discover current provider cycles and execute every due durable job once.
    RunOnce,
    /// Poll forever until Ctrl-C, recovering interrupted jobs on startup.
    Daemon,
    /// Print durable job state without network access.
    Status,
    /// Measure host/filesystem facts without creating roots or contacting providers.
    CapacityAudit {
        /// Override the clock for reproducible operations/tests.
        #[arg(long, hide = true)]
        now_unix: Option<i64>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config = SchedulerConfig::load(&cli.config)?;
    if let Command::CapacityAudit { now_unix } = &cli.command {
        let report =
            audit_host_capacity(&config, now_unix.unwrap_or_else(|| Utc::now().timestamp()))?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let host = Arc::new(SchedulerHost::new(config)?);
    match cli.command {
        Command::Plan { now_unix } => {
            let plans = host.plan_at(now_unix.unwrap_or_else(|| Utc::now().timestamp()))?;
            println!("{}", serde_json::to_string_pretty(&plans)?);
        }
        Command::Discover { now_unix } => {
            let report = host.discover_at(now_unix.unwrap_or_else(|| Utc::now().timestamp()))?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::RunOnce => {
            let report = cancellable_run(Arc::clone(&host)).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Status => {
            println!("{}", serde_json::to_string_pretty(&host.status()?)?);
        }
        Command::CapacityAudit { .. } => unreachable!("handled before scheduler initialization"),
        Command::Daemon => daemon(host).await?,
    }
    Ok(())
}

async fn daemon(host: Arc<SchedulerHost>) -> SchedulerResult<()> {
    loop {
        let report = cancellable_run(Arc::clone(&host)).await?;
        println!("{}", serde_json::to_string(&report)?);
        if host.shutdown_requested() {
            return Ok(());
        }
        let sleep = tokio::time::sleep(Duration::from_secs(host.config().poll_seconds));
        tokio::pin!(sleep);
        tokio::select! {
            _ = &mut sleep => {}
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(SchedulerError::Io)?;
                host.request_shutdown();
                return Ok(());
            }
        }
    }
}

async fn cancellable_run(host: Arc<SchedulerHost>) -> SchedulerResult<ExecutionReport> {
    let worker_host = Arc::clone(&host);
    let mut work = tokio::task::spawn_blocking(move || worker_host.run_once());
    tokio::select! {
        result = &mut work => join_result(result),
        signal = tokio::signal::ctrl_c() => {
            signal.map_err(SchedulerError::Io)?;
            host.request_shutdown();
            join_result(work.await)
        }
    }
}

fn join_result(
    result: Result<SchedulerResult<ExecutionReport>, tokio::task::JoinError>,
) -> SchedulerResult<ExecutionReport> {
    result.map_err(|error| {
        SchedulerError::InvalidState(format!("scheduler worker terminated unexpectedly: {error}"))
    })?
}
