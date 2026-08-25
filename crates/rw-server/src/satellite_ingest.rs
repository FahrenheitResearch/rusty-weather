//! Server-owned, request-independent satellite ingestion lifecycle.
//!
//! The supervisor only orchestrates `rw_sat::follow`: source selection,
//! download validation, native archival, preview generation, and retention
//! remain in rw-sat so the server cannot drift into a second science path.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rw_sat::events::SatEvent;
use rw_sat::follow::{FollowConfig, follow};
use rw_sat::s3::{Sector, bucket_for_satellite};
use rw_sat::window::WindowConfig;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config::{SatelliteFollowSpec, SatelliteIngestConfig, SatelliteSectorConfig};

const RESTART_DELAY: Duration = Duration::from_secs(5);
const CANCEL_POLL_DELAY: Duration = Duration::from_millis(100);

trait FollowRunner: Send + Sync + 'static {
    fn run(&self, config: &FollowConfig, cancel: &AtomicBool) -> Result<(), String>;
}

struct RwSatFollowRunner {
    updates: SatelliteIngestSignal,
}

impl FollowRunner for RwSatFollowRunner {
    fn run(&self, config: &FollowConfig, cancel: &AtomicBool) -> Result<(), String> {
        let platform = config.satellite.clone();
        let sector = config.sector.slug();
        let updates = self.updates.clone();
        let mut sink = move |event| log_follow_event(&platform, sector, &updates, event);
        follow(config, &mut sink, cancel)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

struct FollowerWorker {
    label: String,
    cancel: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

/// Owns every configured blocking rw-sat follower and guarantees cooperative
/// cancellation plus a join before server shutdown completes.
pub struct SatelliteIngestSupervisor {
    workers: Vec<FollowerWorker>,
    updates: SatelliteIngestSignal,
}

/// Coalescing notification that the durable native archive changed. The
/// notification is deliberately only a wake-up hint: consumers rescan the
/// atomic frame manifests, making startup, event coalescing, and a missed wake
/// converge on the same source of truth without an unbounded event queue.
#[derive(Clone, Debug)]
pub struct SatelliteIngestSignal {
    archive_epoch: watch::Sender<u64>,
}

impl Default for SatelliteIngestSignal {
    fn default() -> Self {
        let (archive_epoch, _receiver) = watch::channel(0);
        Self { archive_epoch }
    }
}

impl SatelliteIngestSignal {
    /// Process-local wake epoch only. This is not a source revision and must
    /// never participate in a derivative/cache identity; consumers derive
    /// that from the committed manifest's required-channel content digests.
    pub fn archive_epoch(&self) -> u64 {
        *self.archive_epoch.borrow()
    }

    /// Wait until at least one native manifest commit happened after
    /// `observed_epoch`. Multiple commits may be coalesced into one wake.
    pub async fn changed_after(&self, observed_epoch: u64) -> u64 {
        let mut archive_epoch = self.archive_epoch.subscribe();
        loop {
            let current = *archive_epoch.borrow_and_update();
            if current != observed_epoch {
                return current;
            }
            // `self` retains a sender for the duration of this call.
            archive_epoch
                .changed()
                .await
                .expect("satellite ingest archive-epoch sender is retained");
        }
    }

    fn record_native_update(&self) {
        self.archive_epoch.send_modify(|epoch| {
            *epoch = epoch.wrapping_add(1);
        });
    }
}

impl SatelliteIngestSupervisor {
    /// Start all configured followers. Disabled configuration starts no task
    /// and does not create the staging root.
    pub fn start(config: &SatelliteIngestConfig, store_root: &Path) -> Result<Self, io::Error> {
        let updates = SatelliteIngestSignal::default();
        Self::start_with_runner_and_signal(
            config,
            store_root,
            Arc::new(RwSatFollowRunner {
                updates: updates.clone(),
            }),
            updates,
        )
    }

    #[cfg(test)]
    fn start_with_runner(
        config: &SatelliteIngestConfig,
        store_root: &Path,
        runner: Arc<dyn FollowRunner>,
    ) -> Result<Self, io::Error> {
        Self::start_with_runner_and_signal(
            config,
            store_root,
            runner,
            SatelliteIngestSignal::default(),
        )
    }

    fn start_with_runner_and_signal(
        config: &SatelliteIngestConfig,
        store_root: &Path,
        runner: Arc<dyn FollowRunner>,
        updates: SatelliteIngestSignal,
    ) -> Result<Self, io::Error> {
        if !config.enabled {
            return Ok(Self {
                workers: Vec::new(),
                updates,
            });
        }

        fs::create_dir_all(&config.raw_cache_root)?;
        let mut follow_configs = Vec::with_capacity(config.followers.len());
        for spec in &config.followers {
            let follow_config = build_follow_config(config, spec, store_root)?;
            fs::create_dir_all(&follow_config.cache_dir)?;
            follow_configs.push(follow_config);
        }

        let mut workers = Vec::with_capacity(follow_configs.len());
        for follow_config in follow_configs {
            let label = format!(
                "{}/{}",
                follow_config.satellite,
                follow_config.sector.slug()
            );
            let task_label = label.clone();
            let cancel = Arc::new(AtomicBool::new(false));
            let task_cancel = cancel.clone();
            let task_runner = runner.clone();
            let task = tokio::task::spawn_blocking(move || {
                info!(source = %task_label, "satellite ingest follower started");
                while !task_cancel.load(Ordering::Relaxed) {
                    let result = task_runner.run(&follow_config, &task_cancel);
                    if task_cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    match result {
                        Ok(()) => warn!(
                            source = %task_label,
                            "satellite ingest follower ended unexpectedly; restarting"
                        ),
                        Err(error) => warn!(
                            source = %task_label,
                            %error,
                            "satellite ingest follower failed; restarting"
                        ),
                    }
                    sleep_until_restart_or_cancel(&task_cancel);
                }
                info!(source = %task_label, "satellite ingest follower stopped");
            });
            workers.push(FollowerWorker {
                label,
                cancel,
                task,
            });
        }
        Ok(Self { workers, updates })
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    pub fn update_signal(&self) -> SatelliteIngestSignal {
        self.updates.clone()
    }

    pub fn cancel(&self) {
        for worker in &self.workers {
            worker.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Cooperatively stop and join every blocking follower. rw-sat checks the
    /// same flag during polling, sleeping, and streaming object downloads.
    pub async fn shutdown(&mut self) {
        self.cancel();
        let workers = std::mem::take(&mut self.workers);
        for worker in workers {
            if let Err(error) = worker.task.await {
                warn!(source = %worker.label, %error, "satellite ingest follower join failed");
            }
        }
    }
}

impl Drop for SatelliteIngestSupervisor {
    fn drop(&mut self) {
        // The normal server path awaits `shutdown`; this guard still prevents
        // orphaned work when a later startup step returns early.
        self.cancel();
    }
}

fn build_follow_config(
    config: &SatelliteIngestConfig,
    spec: &SatelliteFollowSpec,
    store_root: &Path,
) -> Result<FollowConfig, io::Error> {
    let sector = sector(spec.sector);
    let bucket = bucket_for_satellite(&spec.platform)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let cache_dir = config.raw_cache_root.join(bucket).join(sector.slug());
    let mut follow_config = FollowConfig::new(&spec.platform, sector, spec.bands.clone());
    follow_config.store_root = store_root.to_path_buf();
    follow_config.cache_dir = cache_dir;
    follow_config.poll_interval = spec.poll_interval_seconds.map(Duration::from_secs);
    follow_config.window = WindowConfig {
        max_age_minutes: spec.retention_max_age_minutes,
        max_bytes: spec.retention_max_bytes,
    };
    // Native NOAA NetCDF remains the artifact of record. rw-sat chooses only
    // the compact preview stride automatically and bounds the raw staging
    // cache using this same rolling retention policy.
    follow_config.downsample = 0;
    follow_config.max_polls = None;
    follow_config.max_frames = None;
    follow_config.use_cache = true;
    Ok(follow_config)
}

fn sector(value: SatelliteSectorConfig) -> Sector {
    match value {
        SatelliteSectorConfig::Conus => Sector::Conus,
        SatelliteSectorConfig::FullDisk => Sector::FullDisk,
        SatelliteSectorConfig::Meso1 => Sector::Meso1,
        SatelliteSectorConfig::Meso2 => Sector::Meso2,
    }
}

fn sleep_until_restart_or_cancel(cancel: &AtomicBool) {
    let deadline = std::time::Instant::now() + RESTART_DELAY;
    while std::time::Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
        std::thread::sleep(
            deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(CANCEL_POLL_DELAY),
        );
    }
}

fn log_follow_event(
    platform: &str,
    sector: &str,
    updates: &SatelliteIngestSignal,
    event: SatEvent,
) {
    match event {
        SatEvent::Warning { message } => {
            warn!(%platform, %sector, %message, "satellite ingest warning")
        }
        SatEvent::FrameWritten {
            scan_time_utc,
            bytes,
            ..
        } => info!(
            %platform,
            %sector,
            scan_time = %scan_time_utc,
            bytes,
            "satellite frame committed"
        ),
        SatEvent::NativeFrameUpdated {
            frame,
            committed_channel,
        } => {
            updates.record_native_update();
            info!(
                %platform,
                %sector,
                frame = %frame.frame_id,
                channel = committed_channel,
                available_channels = frame.channels.len(),
                "native satellite manifest updated"
            );
        }
        SatEvent::Info { message } => {
            debug!(%platform, %sector, %message, "satellite ingest")
        }
        event => debug!(%platform, %sector, ?event, "satellite ingest progress"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use tempfile::tempdir;

    use super::*;

    struct BlockingFakeRunner {
        started: Arc<AtomicUsize>,
        stopped: Arc<AtomicUsize>,
    }

    impl FollowRunner for BlockingFakeRunner {
        fn run(&self, _config: &FollowConfig, cancel: &AtomicBool) -> Result<(), String> {
            self.started.fetch_add(1, Ordering::SeqCst);
            while !cancel.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.stopped.fetch_add(1, Ordering::SeqCst);
            Err(rw_sat::events::SatError::Cancelled.to_string())
        }
    }

    fn enabled_config(root: &Path) -> SatelliteIngestConfig {
        SatelliteIngestConfig {
            enabled: true,
            raw_cache_root: root.join("raw"),
            followers: vec![SatelliteFollowSpec {
                platform: "goes19".into(),
                sector: SatelliteSectorConfig::FullDisk,
                bands: vec![1, 2, 3, 13],
                poll_interval_seconds: Some(60),
                retention_max_age_minutes: Some(24 * 60),
                retention_max_bytes: None,
            }],
        }
    }

    #[tokio::test]
    async fn disabled_supervisor_starts_no_runner_or_staging_directory() {
        let root = tempdir().unwrap();
        let config = SatelliteIngestConfig {
            raw_cache_root: root.path().join("raw"),
            ..SatelliteIngestConfig::default()
        };
        let started = Arc::new(AtomicUsize::new(0));
        let mut supervisor = SatelliteIngestSupervisor::start_with_runner(
            &config,
            &root.path().join("store"),
            Arc::new(BlockingFakeRunner {
                started: started.clone(),
                stopped: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .unwrap();
        assert_eq!(supervisor.worker_count(), 0);
        assert_eq!(started.load(Ordering::SeqCst), 0);
        assert!(!config.raw_cache_root.exists());
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn enabled_supervisor_cancels_and_joins_every_follower() {
        let root = tempdir().unwrap();
        let config = enabled_config(root.path());
        let started = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicUsize::new(0));
        let mut supervisor = SatelliteIngestSupervisor::start_with_runner(
            &config,
            &root.path().join("store"),
            Arc::new(BlockingFakeRunner {
                started: started.clone(),
                stopped: stopped.clone(),
            }),
        )
        .unwrap();
        for _ in 0..100 {
            if started.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(supervisor.worker_count(), 1);
        assert_eq!(started.load(Ordering::SeqCst), 1);
        supervisor.shutdown().await;
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
        assert_eq!(supervisor.worker_count(), 0);
    }

    #[tokio::test]
    async fn ingest_signal_coalesces_updates_without_losing_the_epoch() {
        let signal = SatelliteIngestSignal::default();
        assert_eq!(signal.archive_epoch(), 0);

        signal.record_native_update();
        signal.record_native_update();
        assert_eq!(signal.changed_after(0).await, 2);

        let observed = signal.archive_epoch();
        let waiter = {
            let signal = signal.clone();
            tokio::spawn(async move { signal.changed_after(observed).await })
        };
        tokio::task::yield_now().await;
        signal.record_native_update();
        assert_eq!(waiter.await.unwrap(), 3);
    }

    #[test]
    fn durable_native_frame_event_advances_the_reconcile_signal() {
        let signal = SatelliteIngestSignal::default();
        let channel = rw_sat::archive::NativeChannelSource {
            channel: 13,
            object_key: "fixture/c13.nc".into(),
            relative_path: ".rw-satellite-sources/g19/fulldisk/20260822/20260822T1950/c13.nc"
                .into(),
            byte_size: 1,
            content_blake3: None,
            scan_start_unix: 1_787_450_600,
            scan_end_unix: 1_787_451_200,
        };
        let frame = rw_sat::NativeSatelliteFrame {
            schema: rw_sat::archive::NATIVE_FRAME_SCHEMA.into(),
            platform: "g19".into(),
            sector: "fulldisk".into(),
            frame_id: "20260822T1950".into(),
            scan_start_unix: 1_787_450_600,
            scan_end_unix: 1_787_451_200,
            channels: std::collections::BTreeMap::from([(13, channel)]),
        };

        log_follow_event(
            "goes19",
            "fulldisk",
            &signal,
            SatEvent::NativeFrameUpdated {
                frame,
                committed_channel: 13,
            },
        );

        assert_eq!(signal.archive_epoch(), 1);
    }
}
