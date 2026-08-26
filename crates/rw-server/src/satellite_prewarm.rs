//! Bounded native-satellite tile planning and immutable work identity.
//!
//! This module intentionally does not render or publish tiles. It produces a
//! lazy, breadth-first plan that a worker can feed through the exact HTTP
//! renderer/cache path. The plan retains only configured region rectangles,
//! never a `Vec` containing every planned XYZ tile.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rw_sat::{GoesAbiProduct, list_native_frames, resolve_native_frame_with_revision};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::AppState;
use crate::config::{
    SatelliteHotRegionConfig, SatellitePrewarmConfig, SatellitePrewarmSourceConfig,
    SatelliteSectorConfig,
};
use crate::satellite::{SatellitePrewarmTile, prewarm_revisioned_tile};
use crate::satellite_ingest::SatelliteIngestSignal;

const WEB_MERCATOR_MAX_LATITUDE: f64 = 85.051_128_78;
const PLAN_DIGEST_DOMAIN: &[u8] = b"rw-server.satellite-prewarm-plan.v1\0";

/// Exact XYZ coordinate in the standard Web-Mercator tile pyramid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct XyzTile {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

/// Immutable identity for one product/frame warm operation.
///
/// `minute_frame_id` is intentionally not a source revision: a minute bucket
/// can gain or replace required channels. `source_revision` must come from the
/// committed required-channel content identity. `SatelliteIngestSignal`'s
/// process-local epoch is only a reconcile wake-up and must never be placed in
/// this key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkKey {
    pub renderer_recipe: String,
    pub platform: String,
    pub sector: String,
    pub product: String,
    pub minute_frame_id: String,
    pub source_revision: String,
    pub plan_digest: String,
}

impl WorkKey {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        renderer_recipe: impl Into<String>,
        platform: impl Into<String>,
        sector: impl Into<String>,
        product: impl Into<String>,
        minute_frame_id: impl Into<String>,
        source_revision: impl Into<String>,
        plan_digest: impl Into<String>,
    ) -> Self {
        Self {
            renderer_recipe: renderer_recipe.into(),
            platform: platform.into(),
            sector: sector.into(),
            product: product.into(),
            minute_frame_id: minute_frame_id.into(),
            source_revision: source_revision.into(),
            plan_digest: plan_digest.into(),
        }
    }
}

const STATUS_SCHEMA: &str = "rw-server.satellite-prewarm-status.v1";
const MAX_COMPLETED_WORK_KEYS: usize = 4_096;
const MAX_STATUS_ERROR_BYTES: usize = 2_048;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SatellitePrewarmPhase {
    #[default]
    Disabled,
    WaitingForSource,
    Reconciling,
    Rendering,
    Ready,
    Degraded,
    Stopped,
}

#[derive(Clone, Debug, Serialize)]
pub struct SatellitePrewarmStatus {
    pub schema: &'static str,
    pub enabled: bool,
    pub ready: bool,
    pub phase: SatellitePrewarmPhase,
    pub active_work: Option<WorkKey>,
    pub configured_sources: usize,
    pub waiting_sources: usize,
    pub reconcile_count: u64,
    pub planned_tiles: u64,
    pub completed_tiles: u64,
    pub failed_tiles: u64,
    pub completed_product_frames: u64,
    pub last_reconcile_unix_ms: Option<i64>,
    pub last_success_unix_ms: Option<i64>,
    pub last_error: Option<String>,
}

impl SatellitePrewarmStatus {
    fn new(enabled: bool, configured_sources: usize) -> Self {
        Self {
            schema: STATUS_SCHEMA,
            enabled,
            ready: !enabled,
            phase: SatellitePrewarmPhase::Disabled,
            active_work: None,
            configured_sources,
            waiting_sources: 0,
            reconcile_count: 0,
            planned_tiles: 0,
            completed_tiles: 0,
            failed_tiles: 0,
            completed_product_frames: 0,
            last_reconcile_unix_ms: None,
            last_success_unix_ms: None,
            last_error: None,
        }
    }
}

/// Cheap cloneable status cell shared with the protected HTTP endpoint.
#[derive(Clone, Debug)]
pub struct SatellitePrewarmStatusHandle {
    inner: Arc<RwLock<SatellitePrewarmStatus>>,
}

impl SatellitePrewarmStatusHandle {
    pub fn new(config: &SatellitePrewarmConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(SatellitePrewarmStatus::new(
                config.enabled,
                config.sources.len(),
            ))),
        }
    }

    pub fn snapshot(&self) -> SatellitePrewarmStatus {
        self.inner
            .read()
            .map(|status| status.clone())
            .unwrap_or_else(|_| SatellitePrewarmStatus {
                last_error: Some("satellite prewarm status lock is poisoned".to_owned()),
                phase: SatellitePrewarmPhase::Degraded,
                ready: false,
                ..SatellitePrewarmStatus::new(true, 0)
            })
    }

    fn update(&self, operation: impl FnOnce(&mut SatellitePrewarmStatus)) {
        if let Ok(mut status) = self.inner.write() {
            operation(&mut status);
        }
    }
}

/// Owns the low-priority, request-independent tile worker and joins it during
/// graceful shutdown. One tile is processed at a time; the server's heavy
/// semaphore remains the final shared admission boundary.
pub struct SatellitePrewarmSupervisor {
    cancel: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}

impl SatellitePrewarmSupervisor {
    pub fn start(
        config: SatellitePrewarmConfig,
        state: AppState,
        updates: SatelliteIngestSignal,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        if !config.enabled {
            return Self { cancel, task: None };
        }
        let worker_cancel = cancel.clone();
        let status = state.satellite_prewarm_status.clone();
        let task = tokio::spawn(async move {
            run_worker(config, state, updates, worker_cancel, status).await;
        });
        Self {
            cancel,
            task: Some(task),
        }
    }

    pub async fn shutdown(&mut self) {
        self.cancel.store(true, AtomicOrdering::Release);
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
        {
            warn!(%error, "satellite prewarm worker join failed");
        }
    }
}

impl Drop for SatellitePrewarmSupervisor {
    fn drop(&mut self) {
        self.cancel.store(true, AtomicOrdering::Release);
    }
}

async fn run_worker(
    config: SatellitePrewarmConfig,
    state: AppState,
    updates: SatelliteIngestSignal,
    cancel: Arc<AtomicBool>,
    status: SatellitePrewarmStatusHandle,
) {
    info!(
        sources = config.sources.len(),
        "satellite tile prewarm worker started"
    );
    let mut observed_epoch = updates.archive_epoch();
    let mut completed = HashSet::<WorkKey>::new();
    loop {
        if cancel.load(AtomicOrdering::Acquire) {
            break;
        }
        reconcile(&config, &state, &cancel, &status, &mut completed).await;
        if cancel.load(AtomicOrdering::Acquire) {
            break;
        }
        tokio::select! {
            epoch = updates.changed_after(observed_epoch) => observed_epoch = epoch,
            _ = tokio::time::sleep(Duration::from_secs(config.reconcile_seconds)) => {},
            _ = wait_for_cancel(cancel.clone()) => break,
        }
    }
    status.update(|value| {
        value.active_work = None;
        value.ready = false;
        value.phase = SatellitePrewarmPhase::Stopped;
    });
    info!("satellite tile prewarm worker stopped");
}

async fn wait_for_cancel(cancel: Arc<AtomicBool>) {
    while !cancel.load(AtomicOrdering::Acquire) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn reconcile(
    config: &SatellitePrewarmConfig,
    state: &AppState,
    cancel: &AtomicBool,
    status: &SatellitePrewarmStatusHandle,
    completed: &mut HashSet<WorkKey>,
) {
    status.update(|value| {
        value.phase = SatellitePrewarmPhase::Reconciling;
        value.ready = false;
        value.waiting_sources = 0;
        value.last_error = None;
        value.reconcile_count = value.reconcile_count.saturating_add(1);
        value.last_reconcile_unix_ms = Some(now_unix_ms());
    });
    let mut saw_complete_frame = false;
    let mut degraded = false;
    let mut waiting_sources = 0usize;

    for source in &config.sources {
        if cancel.load(AtomicOrdering::Acquire) {
            return;
        }
        let platform = canonical_platform(&source.platform);
        let sector = sector_slug(source.sector).to_owned();
        let plan = TilePlan::for_source(source);
        let plan_digest = plan.digest();
        let mut source_has_complete_frame = false;

        for product_slug in &source.products {
            let Some(product) = GoesAbiProduct::parse(product_slug) else {
                record_error(
                    status,
                    format!("unsupported configured product {product_slug}"),
                );
                degraded = true;
                continue;
            };
            let lookup_state = state.clone();
            let lookup_platform = platform.clone();
            let lookup_sector = sector.clone();
            let lookup_root = state.config.server.store_root.clone();
            let frame_limit = source.frames_per_product;
            let resolved = lookup_state
                .run_heavy_sync(move || {
                    list_native_frames(
                        &lookup_root,
                        &lookup_platform,
                        &lookup_sector,
                        product,
                        frame_limit,
                    )?
                    .into_iter()
                    .map(|frame| {
                        resolve_native_frame_with_revision(
                            &lookup_root,
                            &lookup_platform,
                            &lookup_sector,
                            product,
                            &frame.frame_id,
                        )
                    })
                    .collect::<std::io::Result<Vec<_>>>()
                })
                .await;
            let frames = match resolved {
                Ok(Ok(frames)) => frames,
                Ok(Err(error)) => {
                    record_error(
                        status,
                        format!("prewarm archive scan failed for {platform}/{sector}: {error}"),
                    );
                    degraded = true;
                    continue;
                }
                Err(error) => {
                    record_error(
                        status,
                        format!("prewarm worker unavailable for {platform}/{sector}: {error}"),
                    );
                    degraded = true;
                    continue;
                }
            };
            if frames.is_empty() {
                continue;
            }
            source_has_complete_frame = true;
            saw_complete_frame = true;

            for resolved in frames {
                let key = WorkKey::new(
                    crate::satellite::SATELLITE_TILE_RECIPE_VERSION,
                    &platform,
                    &sector,
                    product.slug(),
                    &resolved.frame.frame_id,
                    &resolved.source_revision,
                    &plan_digest,
                );
                if completed.contains(&key) {
                    continue;
                }
                status.update(|value| {
                    value.phase = SatellitePrewarmPhase::Rendering;
                    value.active_work = Some(key.clone());
                    value.planned_tiles = value.planned_tiles.saturating_add(plan.tile_count());
                });
                let mut work_complete = true;
                for tile in plan.iter() {
                    if cancel.load(AtomicOrdering::Acquire) {
                        return;
                    }
                    let result = prewarm_revisioned_tile(
                        state.clone(),
                        SatellitePrewarmTile {
                            platform: platform.clone(),
                            sector: sector.clone(),
                            product,
                            frame: resolved.frame.frame_id.clone(),
                            source_revision: resolved.source_revision.clone(),
                            z: tile.z,
                            x: tile.x,
                            y: tile.y,
                        },
                    )
                    .await;
                    match result {
                        Ok(()) => status.update(|value| {
                            value.completed_tiles = value.completed_tiles.saturating_add(1);
                            value.last_success_unix_ms = Some(now_unix_ms());
                        }),
                        Err(error) => {
                            status.update(|value| {
                                value.failed_tiles = value.failed_tiles.saturating_add(1);
                            });
                            record_error(
                                status,
                                format!(
                                    "prewarm failed for {}/{}/{}/{}/{}/{}/{}: {error}",
                                    platform,
                                    sector,
                                    product.slug(),
                                    resolved.frame.frame_id,
                                    tile.z,
                                    tile.x,
                                    tile.y
                                ),
                            );
                            work_complete = false;
                            degraded = true;
                            break;
                        }
                    }
                    tokio::task::yield_now().await;
                }
                if work_complete {
                    completed.insert(key);
                    status.update(|value| {
                        value.completed_product_frames =
                            value.completed_product_frames.saturating_add(1);
                    });
                }
                if completed.len() > MAX_COMPLETED_WORK_KEYS {
                    // Exact durable entries are still the source of truth; a
                    // later reconciliation merely revalidates them on disk.
                    completed.clear();
                }
            }
        }
        if !source_has_complete_frame {
            waiting_sources = waiting_sources.saturating_add(1);
        }
    }

    status.update(|value| {
        value.active_work = None;
        value.waiting_sources = waiting_sources;
        value.ready = saw_complete_frame && !degraded;
        value.phase = if degraded {
            SatellitePrewarmPhase::Degraded
        } else if saw_complete_frame {
            SatellitePrewarmPhase::Ready
        } else {
            SatellitePrewarmPhase::WaitingForSource
        };
    });
}

fn canonical_platform(value: &str) -> String {
    rw_sat::goes::GoesSatellite::parse(value)
        .as_str()
        .to_ascii_lowercase()
}

const fn sector_slug(sector: SatelliteSectorConfig) -> &'static str {
    match sector {
        SatelliteSectorConfig::Conus => "conus",
        SatelliteSectorConfig::FullDisk => "fulldisk",
        SatelliteSectorConfig::Meso1 => "meso1",
        SatelliteSectorConfig::Meso2 => "meso2",
    }
}

fn record_error(status: &SatellitePrewarmStatusHandle, error: String) {
    let mut error = error;
    if error.len() > MAX_STATUS_ERROR_BYTES {
        error.truncate(MAX_STATUS_ERROR_BYTES);
    }
    warn!(%error, "satellite tile prewarm degraded");
    status.update(|value| value.last_error = Some(error));
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TileRect {
    min_x: u32,
    max_x: u32,
    min_y: u32,
    max_y: u32,
}

impl TileRect {
    fn contains(self, x: u32, y: u32) -> bool {
        (self.min_x..=self.max_x).contains(&x) && (self.min_y..=self.max_y).contains(&y)
    }
}

/// Lazily iterable breadth-first plan for one product/frame.
#[derive(Clone, Debug)]
pub struct TilePlan {
    overview_max_zoom: u8,
    hot_regions: Vec<SatelliteHotRegionConfig>,
    max_zoom: u8,
}

impl TilePlan {
    /// Configuration validation runs before this constructor. Keeping the
    /// constructor crate-visible prevents callers from silently normalizing an
    /// invalid public configuration.
    pub(crate) fn for_source(source: &SatellitePrewarmSourceConfig) -> Self {
        let max_zoom = source
            .hot_regions
            .iter()
            .map(|region| region.max_zoom)
            .max()
            .unwrap_or(source.overview_max_zoom)
            .max(source.overview_max_zoom);
        Self {
            overview_max_zoom: source.overview_max_zoom,
            hot_regions: source.hot_regions.clone(),
            max_zoom,
        }
    }

    /// Exact number of unique XYZ coordinates produced by this plan.
    pub fn tile_count(&self) -> u64 {
        (0..=self.max_zoom)
            .map(|zoom| union_tile_count(&self.rectangles_at(zoom)))
            .sum()
    }

    /// Stable digest of the normalized plan configuration. Region ordering
    /// does not affect the digest. The work identity also carries renderer,
    /// source, product, frame minute, and source-content revision separately.
    pub fn digest(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(PLAN_DIGEST_DOMAIN);
        hasher.update(&[self.overview_max_zoom]);
        let mut regions = self
            .hot_regions
            .iter()
            .map(|region| CanonicalRegion {
                west: canonical_f64_bits(region.west),
                south: canonical_f64_bits(region.south),
                east: canonical_f64_bits(region.east),
                north: canonical_f64_bits(region.north),
                max_zoom: region.max_zoom,
            })
            .collect::<Vec<_>>();
        regions.sort_unstable();
        for region in regions {
            hasher.update(&region.west.to_le_bytes());
            hasher.update(&region.south.to_le_bytes());
            hasher.update(&region.east.to_le_bytes());
            hasher.update(&region.north.to_le_bytes());
            hasher.update(&[region.max_zoom]);
        }
        hasher.finalize().to_hex().to_string()
    }

    pub fn iter(&self) -> TilePlanIter {
        TilePlanIter::new(self.clone())
    }

    fn rectangles_at(&self, zoom: u8) -> Vec<TileRect> {
        let tile_span = 1_u32 << zoom;
        if zoom <= self.overview_max_zoom {
            return vec![TileRect {
                min_x: 0,
                max_x: tile_span - 1,
                min_y: 0,
                max_y: tile_span - 1,
            }];
        }

        let mut rectangles = Vec::with_capacity(self.hot_regions.len().saturating_mul(2));
        for region in &self.hot_regions {
            if zoom <= region.max_zoom {
                rectangles.extend(region_rectangles(*region, zoom));
            }
        }
        rectangles
    }
}

impl IntoIterator for &TilePlan {
    type Item = XyzTile;
    type IntoIter = TilePlanIter;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator state is proportional to configured hot regions, not tile count.
pub struct TilePlanIter {
    plan: TilePlan,
    zoom: u8,
    rectangles: Vec<TileRect>,
    rectangle_index: usize,
    x: u32,
    y: u32,
    finished: bool,
}

impl TilePlanIter {
    fn new(plan: TilePlan) -> Self {
        let rectangles = plan.rectangles_at(0);
        let first = rectangles.first().copied();
        Self {
            plan,
            zoom: 0,
            rectangles,
            rectangle_index: 0,
            x: first.map_or(0, |rectangle| rectangle.min_x),
            y: first.map_or(0, |rectangle| rectangle.min_y),
            finished: false,
        }
    }

    fn advance_candidate(&mut self, rectangle: TileRect) {
        if self.x < rectangle.max_x {
            self.x += 1;
        } else if self.y < rectangle.max_y {
            self.x = rectangle.min_x;
            self.y += 1;
        } else {
            self.rectangle_index += 1;
            if let Some(next) = self.rectangles.get(self.rectangle_index) {
                self.x = next.min_x;
                self.y = next.min_y;
            }
        }
    }

    fn advance_zoom(&mut self) -> bool {
        while self.zoom < self.plan.max_zoom {
            self.zoom += 1;
            self.rectangles = self.plan.rectangles_at(self.zoom);
            self.rectangle_index = 0;
            if let Some(first) = self.rectangles.first() {
                self.x = first.min_x;
                self.y = first.min_y;
                return true;
            }
        }
        false
    }
}

impl Iterator for TilePlanIter {
    type Item = XyzTile;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.finished {
            let Some(rectangle) = self.rectangles.get(self.rectangle_index).copied() else {
                if self.advance_zoom() {
                    continue;
                }
                self.finished = true;
                return None;
            };

            let candidate = XyzTile {
                z: self.zoom,
                x: self.x,
                y: self.y,
            };
            let candidate_rectangle_index = self.rectangle_index;
            self.advance_candidate(rectangle);

            // Rectangle overlap is deduplicated lazily. Antimeridian splits
            // from one region never overlap each other, and any overlap with a
            // prior configured rectangle is emitted only by that prior one.
            if self.rectangles[..candidate_rectangle_index]
                .iter()
                .any(|prior| prior.contains(candidate.x, candidate.y))
            {
                continue;
            }
            return Some(candidate);
        }
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CanonicalRegion {
    west: u64,
    south: u64,
    east: u64,
    north: u64,
    max_zoom: u8,
}

impl Ord for CanonicalRegion {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.west, self.south, self.east, self.north, self.max_zoom).cmp(&(
            other.west,
            other.south,
            other.east,
            other.north,
            other.max_zoom,
        ))
    }
}

impl PartialOrd for CanonicalRegion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn canonical_f64_bits(value: f64) -> u64 {
    if value == 0.0 { 0 } else { value.to_bits() }
}

fn region_rectangles(region: SatelliteHotRegionConfig, zoom: u8) -> Vec<TileRect> {
    let (min_y, max_y) = latitude_tile_range(region.south, region.north, zoom);
    if region.west < region.east {
        let (min_x, max_x) = longitude_tile_range(region.west, region.east, zoom);
        vec![TileRect {
            min_x,
            max_x,
            min_y,
            max_y,
        }]
    } else {
        let (west_min_x, west_max_x) = longitude_tile_range(region.west, 180.0, zoom);
        let (east_min_x, east_max_x) = longitude_tile_range(-180.0, region.east, zoom);
        vec![
            TileRect {
                min_x: west_min_x,
                max_x: west_max_x,
                min_y,
                max_y,
            },
            TileRect {
                min_x: east_min_x,
                max_x: east_max_x,
                min_y,
                max_y,
            },
        ]
    }
}

fn longitude_tile_range(west: f64, east: f64, zoom: u8) -> (u32, u32) {
    debug_assert!(west < east);
    let tile_span = 1_u32 << zoom;
    let span = f64::from(tile_span);
    let start = (((west + 180.0) / 360.0) * span)
        .floor()
        .clamp(0.0, span - 1.0) as u32;
    let end_exclusive = (((east + 180.0) / 360.0) * span).ceil().clamp(1.0, span) as u32;
    (start, end_exclusive.saturating_sub(1).max(start))
}

fn latitude_tile_range(south: f64, north: f64, zoom: u8) -> (u32, u32) {
    debug_assert!(south < north);
    let tile_span = 1_u32 << zoom;
    let span = f64::from(tile_span);
    let north_y = mercator_tile_y(north, span).floor().clamp(0.0, span - 1.0) as u32;
    let south_exclusive = mercator_tile_y(south, span).ceil().clamp(1.0, span) as u32;
    (north_y, south_exclusive.saturating_sub(1).max(north_y))
}

fn mercator_tile_y(latitude: f64, tile_span: f64) -> f64 {
    let latitude = latitude.clamp(-WEB_MERCATOR_MAX_LATITUDE, WEB_MERCATOR_MAX_LATITUDE);
    let radians = latitude.to_radians();
    (1.0 - radians.tan().asinh() / std::f64::consts::PI) * 0.5 * tile_span
}

fn union_tile_count(rectangles: &[TileRect]) -> u64 {
    if rectangles.is_empty() {
        return 0;
    }
    let mut x_edges = Vec::with_capacity(rectangles.len().saturating_mul(2));
    for rectangle in rectangles {
        x_edges.push(rectangle.min_x);
        x_edges.push(rectangle.max_x + 1);
    }
    x_edges.sort_unstable();
    x_edges.dedup();

    let mut total = 0_u64;
    for x_window in x_edges.windows(2) {
        let start_x = x_window[0];
        let end_x = x_window[1];
        let mut y_intervals = rectangles
            .iter()
            .filter(|rectangle| rectangle.min_x < end_x && rectangle.max_x >= start_x)
            .map(|rectangle| (rectangle.min_y, rectangle.max_y))
            .collect::<Vec<_>>();
        y_intervals.sort_unstable();
        let mut covered_y = 0_u64;
        let mut current: Option<(u32, u32)> = None;
        for (start_y, end_y) in y_intervals {
            match current {
                Some((current_start, current_end)) if start_y <= current_end.saturating_add(1) => {
                    current = Some((current_start, current_end.max(end_y)));
                }
                Some((current_start, current_end)) => {
                    covered_y += u64::from(current_end - current_start + 1);
                    current = Some((start_y, end_y));
                }
                None => current = Some((start_y, end_y)),
            }
        }
        if let Some((start_y, end_y)) = current {
            covered_y += u64::from(end_y - start_y + 1);
        }
        total += u64::from(end_x - start_x) * covered_y;
    }
    total
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::time::Duration;

    use bytes::Bytes;
    use rw_sat::archive::{NATIVE_FRAME_SCHEMA, NativeChannelSource};

    use crate::config::{AppConfig, SatelliteSectorConfig};
    use crate::state::{CachedSatelliteTile, SatelliteTileCacheKey};
    use crate::{AppState, TokenSet};

    use super::*;

    fn source(
        overview_max_zoom: u8,
        hot_regions: Vec<SatelliteHotRegionConfig>,
    ) -> SatellitePrewarmSourceConfig {
        SatellitePrewarmSourceConfig {
            platform: "goes19".into(),
            sector: SatelliteSectorConfig::FullDisk,
            products: vec!["geocolor".into()],
            frames_per_product: 3,
            overview_max_zoom,
            hot_regions,
        }
    }

    #[test]
    fn overview_is_exact_and_breadth_first_without_materializing_tiles() {
        let plan = TilePlan::for_source(&source(3, Vec::new()));
        assert_eq!(plan.tile_count(), 1 + 4 + 16 + 64);
        let tiles = plan.iter().collect::<Vec<_>>();
        assert_eq!(tiles.len() as u64, plan.tile_count());
        assert!(tiles.windows(2).all(|pair| pair[0].z <= pair[1].z));
        assert_eq!(tiles[0], XyzTile { z: 0, x: 0, y: 0 });
        assert_eq!(tiles.last().unwrap().z, 3);
    }

    #[test]
    fn antimeridian_region_warms_both_edges_and_not_the_middle() {
        let region = SatelliteHotRegionConfig {
            west: 170.0,
            south: -10.0,
            east: -170.0,
            north: 10.0,
            max_zoom: 3,
        };
        let plan = TilePlan::for_source(&source(0, vec![region]));
        let zoom_three = plan.iter().filter(|tile| tile.z == 3).collect::<Vec<_>>();
        assert!(!zoom_three.is_empty());
        assert!(zoom_three.iter().all(|tile| tile.x == 0 || tile.x == 7));
        assert!(zoom_three.iter().any(|tile| tile.x == 0));
        assert!(zoom_three.iter().any(|tile| tile.x == 7));
    }

    #[test]
    fn overlapping_hot_regions_emit_each_xyz_coordinate_exactly_once() {
        let first = SatelliteHotRegionConfig {
            west: -120.0,
            south: 20.0,
            east: -70.0,
            north: 55.0,
            max_zoom: 5,
        };
        let second = SatelliteHotRegionConfig {
            west: -100.0,
            south: 30.0,
            east: -60.0,
            north: 60.0,
            max_zoom: 5,
        };
        let plan = TilePlan::for_source(&source(1, vec![first, second, first]));
        let tiles = plan.iter().collect::<Vec<_>>();
        let unique = tiles.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(tiles.len(), unique.len());
        assert_eq!(tiles.len() as u64, plan.tile_count());
        assert!(tiles.windows(2).all(|pair| pair[0].z <= pair[1].z));
    }

    #[test]
    fn plan_digest_is_stable_across_region_order_but_changes_with_coverage() {
        let first = SatelliteHotRegionConfig {
            west: -120.0,
            south: 20.0,
            east: -70.0,
            north: 55.0,
            max_zoom: 5,
        };
        let second = SatelliteHotRegionConfig {
            west: 160.0,
            south: -20.0,
            east: -160.0,
            north: 20.0,
            max_zoom: 4,
        };
        let one = TilePlan::for_source(&source(1, vec![first, second]));
        let reordered = TilePlan::for_source(&source(1, vec![second, first]));
        let changed = TilePlan::for_source(&source(2, vec![second, first]));
        assert_eq!(one.digest(), reordered.digest());
        assert_ne!(one.digest(), changed.digest());
    }

    #[test]
    fn work_key_never_conflates_frame_minute_with_source_revision() {
        let base = WorkKey::new(
            "rw-sat-native-v1",
            "g19",
            "fulldisk",
            "geocolor",
            "20260822T1950",
            "required-channel-content-a",
            "plan-a",
        );
        let mut replaced_source = base.clone();
        replaced_source.source_revision = "required-channel-content-b".into();
        let mut changed_plan = base.clone();
        changed_plan.plan_digest = "plan-b".into();

        assert_ne!(base, replaced_source);
        assert_ne!(base, changed_plan);
        assert_eq!(base.minute_frame_id, replaced_source.minute_frame_id);
    }

    #[tokio::test]
    async fn worker_reconciles_a_complete_frame_through_the_durable_cache() {
        const FRAME: &str = "20260822T1951";
        const TINY_PNG: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x60, 0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x05, 0x00, 0x01, 0xa5, 0xf6,
            0x45, 0x40, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let directory = tempfile::tempdir().unwrap();
        let mut config = AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        write_clean_ir_frame(&config.server.store_root, FRAME);
        config.satellite_prewarm = SatellitePrewarmConfig {
            enabled: true,
            maximum_tiles_per_product_frame: 1,
            reconcile_seconds: 5,
            sources: vec![SatellitePrewarmSourceConfig {
                platform: "goes18".into(),
                sector: SatelliteSectorConfig::FullDisk,
                products: vec!["clean_ir".into()],
                frames_per_product: 1,
                overview_max_zoom: 0,
                hot_regions: Vec::new(),
            }],
        };
        config.validate(false).unwrap();
        let state = AppState::new(config.clone(), TokenSet::default()).unwrap();
        let resolved = resolve_native_frame_with_revision(
            &config.server.store_root,
            "g18",
            "fulldisk",
            GoesAbiProduct::CleanInfrared,
            FRAME,
        )
        .unwrap();
        let key = SatelliteTileCacheKey {
            recipe: crate::satellite::SATELLITE_TILE_RECIPE_VERSION.into(),
            source_revision: resolved.source_revision.clone(),
            platform: "g18".into(),
            sector: "fulldisk".into(),
            product: "clean_ir".into(),
            frame: FRAME.into(),
            zoom: 0,
            x: 0,
            y: 0,
            tile_size: rw_sat::DEFAULT_TILE_SIZE,
        };
        let png = Bytes::from_static(TINY_PNG);
        state
            .satellite_tile_disk_cache
            .store(
                &key,
                &CachedSatelliteTile {
                    etag: format!("\"{}\"", blake3::hash(&png).to_hex()),
                    png,
                    frame_id: FRAME.into(),
                    source_revision: resolved.source_revision,
                    valid_unix: 1_777_000_000,
                },
            )
            .unwrap();

        let mut supervisor = SatellitePrewarmSupervisor::start(
            config.satellite_prewarm,
            state.clone(),
            SatelliteIngestSignal::default(),
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let status = state.satellite_prewarm_status.snapshot();
            if status.ready {
                assert_eq!(status.phase, SatellitePrewarmPhase::Ready);
                assert_eq!(status.completed_tiles, 1);
                assert_eq!(status.completed_product_frames, 1);
                assert!(status.last_error.is_none());
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "{status:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        supervisor.shutdown().await;
        assert_eq!(
            state.satellite_prewarm_status.snapshot().phase,
            SatellitePrewarmPhase::Stopped
        );
    }

    fn write_clean_ir_frame(store_root: &std::path::Path, frame_id: &str) {
        let source_bytes = b"fixture native source";
        let digest = blake3::hash(source_bytes).to_hex().to_string();
        let relative_path = format!(
            ".rw-satellite-sources/g18/fulldisk/{}/{frame_id}/c13-{digest}.nc",
            &frame_id[..8]
        );
        let channel = NativeChannelSource {
            channel: 13,
            object_key: format!("fixture/{frame_id}/c13.nc"),
            relative_path: relative_path.clone(),
            byte_size: u64::try_from(source_bytes.len()).unwrap(),
            content_blake3: Some(digest),
            scan_start_unix: 1_777_000_000,
            scan_end_unix: 1_777_000_600,
        };
        let manifest = rw_sat::NativeSatelliteFrame {
            schema: NATIVE_FRAME_SCHEMA.into(),
            platform: "g18".into(),
            sector: "fulldisk".into(),
            frame_id: frame_id.into(),
            scan_start_unix: channel.scan_start_unix,
            scan_end_unix: channel.scan_end_unix,
            channels: std::collections::BTreeMap::from([(13, channel)]),
            l2_products: std::collections::BTreeMap::new(),
        };
        let frame_root = rw_sat::native_archive_root(store_root)
            .join("g18")
            .join("fulldisk")
            .join(&frame_id[..8])
            .join(frame_id);
        fs::create_dir_all(&frame_root).unwrap();
        let source_path = store_root.join(&relative_path);
        fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        fs::write(source_path, source_bytes).unwrap();
        fs::write(
            frame_root.join("frame.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }
}
