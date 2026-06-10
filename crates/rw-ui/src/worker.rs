//! Background store IO: a single worker thread owning all file access so the
//! UI thread never blocks on open/read. Requests go in over a channel,
//! plain-data responses come back; the host polls [`StoreWorker::try_recv`]
//! once per frame and calls the `notify` hook (typically
//! `egui::Context::request_repaint`) to wake the UI when a response lands.
//!
//! The worker keeps a one-entry cache of the open [`HourReader`] and the
//! run's [`GridFile`], so hour scrubbing inside one run only decodes chunks.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use rw_store::grid::GridFile;
use rw_store::reader::HourReader;

use crate::colormap::finite_min_max;
use crate::store_view::{StoreTree, StoreView};

/// One forecast hour of one model run.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HourKey {
    pub model: String,
    pub run: String,
    pub hour: u16,
}

impl std::fmt::Display for HourKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{} f{:03}", self.model, self.run, self.hour)
    }
}

/// One 2D variable of one forecast hour.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldKey {
    pub hour: HourKey,
    pub var: String,
}

/// Variable kind, mirrored from the hour meta's `kind` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    Surface2D,
    Pressure3D,
}

/// What the UI needs to list a variable.
#[derive(Debug, Clone, PartialEq)]
pub struct VarInfo {
    pub name: String,
    pub units: String,
    pub kind: VarKind,
    /// Pressure levels (descending) for 3D variables; empty for 2D.
    pub levels_hpa: Vec<u16>,
}

/// A loaded 2D field ready for false-color display.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldData {
    pub key: FieldKey,
    pub units: String,
    pub nx: usize,
    pub ny: usize,
    /// Row-major `ny * nx`, grid order (row 0 first).
    pub values: Vec<f32>,
    /// Finite value range; `None` when the field is all-NaN.
    pub range: Option<(f32, f32)>,
}

/// One 3D variable's profile at a point.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileVar {
    pub name: String,
    pub units: String,
    /// Descending pressure levels, parallel to `values`.
    pub levels_hpa: Vec<u16>,
    pub values: Vec<f32>,
}

/// Profiles of every 3D variable at one clicked point.
#[derive(Debug, Clone, PartialEq)]
pub struct SoundingData {
    pub hour: HourKey,
    /// Fractional grid coordinates of the request.
    pub fx: f64,
    pub fy: f64,
    /// Coordinates of the nearest grid point, when the grid file is
    /// readable.
    pub lat: Option<f32>,
    pub lon: Option<f32>,
    pub vars: Vec<ProfileVar>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StoreRequest {
    /// Re-scan the store root.
    Enumerate,
    /// Open an hour and report its variables.
    LoadHour(HourKey),
    /// Read a full 2D field.
    LoadField(FieldKey),
    /// Read profiles of all 3D variables at fractional grid coords.
    LoadSounding { hour: HourKey, fx: f64, fy: f64 },
}

/// Worker results. Errors arrive as display strings — the UI only shows
/// them.
#[derive(Debug, Clone, PartialEq)]
pub enum StoreResponse {
    Tree(StoreTree),
    HourVars(HourKey, Result<Vec<VarInfo>, String>),
    Field(FieldKey, Result<FieldData, String>),
    Sounding(HourKey, Result<SoundingData, String>),
}

/// Handle to the worker thread. Dropping it closes the request channel and
/// the thread exits on its own.
pub struct StoreWorker {
    tx: Sender<StoreRequest>,
    rx: Receiver<StoreResponse>,
    _thread: JoinHandle<()>,
}

impl StoreWorker {
    /// Spawn the worker over `view`. `notify` is called after every response
    /// is queued (pass `move || ctx.request_repaint()` from an egui host).
    pub fn spawn(view: StoreView, notify: impl Fn() + Send + 'static) -> Self {
        let (req_tx, req_rx) = channel::<StoreRequest>();
        let (resp_tx, resp_rx) = channel::<StoreResponse>();
        let thread = std::thread::Builder::new()
            .name("rw-ui-store-worker".to_string())
            .spawn(move || worker_loop(view, &req_rx, &resp_tx, &notify))
            .expect("spawn rw-ui store worker thread");
        Self {
            tx: req_tx,
            rx: resp_rx,
            _thread: thread,
        }
    }

    /// Queue a request. Silently drops it if the worker died (the UI keeps
    /// running; pending states just never resolve).
    pub fn send(&self, request: StoreRequest) {
        let _ = self.tx.send(request);
    }

    /// Non-blocking poll for the next response (call once per frame, drain
    /// in a loop).
    pub fn try_recv(&self) -> Option<StoreResponse> {
        self.rx.try_recv().ok()
    }

    /// Blocking poll with a timeout — for tests, not for UI frames.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<StoreResponse> {
        match self.rx.recv_timeout(timeout) {
            Ok(response) => Some(response),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
        }
    }
}

/// Worker-side caches: the most recently opened hour and the current run's
/// grid file.
struct WorkerState {
    view: StoreView,
    hour: Option<(HourKey, HourReader)>,
    grid: Option<((String, String), GridFile)>,
}

fn worker_loop(
    view: StoreView,
    requests: &Receiver<StoreRequest>,
    responses: &Sender<StoreResponse>,
    notify: &(impl Fn() + Send + 'static),
) {
    let mut state = WorkerState {
        view,
        hour: None,
        grid: None,
    };
    loop {
        // Block for the next request, then drain the queue and coalesce:
        // only the LAST of each request kind survives, so scrubbing through
        // hours/variables never builds a backlog of stale loads.
        let first = match requests.recv() {
            Ok(request) => request,
            Err(_) => return, // StoreWorker dropped
        };
        let mut batch = vec![first];
        loop {
            match requests.try_recv() {
                Ok(request) => batch.push(request),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        for request in coalesce(batch) {
            let response = handle(&mut state, request);
            if responses.send(response).is_err() {
                return; // StoreWorker dropped
            }
            notify();
        }
    }
}

/// Keep only the last request of each kind, in stable kind order
/// (enumerate, hour, field, sounding) so dependent loads stay sequenced.
fn coalesce(batch: Vec<StoreRequest>) -> Vec<StoreRequest> {
    let mut enumerate = None;
    let mut hour = None;
    let mut field = None;
    let mut sounding = None;
    for request in batch {
        match request {
            StoreRequest::Enumerate => enumerate = Some(request),
            StoreRequest::LoadHour(_) => hour = Some(request),
            StoreRequest::LoadField(_) => field = Some(request),
            StoreRequest::LoadSounding { .. } => sounding = Some(request),
        }
    }
    [enumerate, hour, field, sounding].into_iter().flatten().collect()
}

fn handle(state: &mut WorkerState, request: StoreRequest) -> StoreResponse {
    match request {
        StoreRequest::Enumerate => StoreResponse::Tree(state.view.enumerate()),
        StoreRequest::LoadHour(key) => {
            let result = hour_vars(state, &key).map_err(|err| err.to_string());
            StoreResponse::HourVars(key, result)
        }
        StoreRequest::LoadField(key) => {
            let result = load_field(state, &key).map_err(|err| err.to_string());
            StoreResponse::Field(key, result)
        }
        StoreRequest::LoadSounding { hour, fx, fy } => {
            let result = load_sounding(state, &hour, fx, fy).map_err(|err| err.to_string());
            StoreResponse::Sounding(hour, result)
        }
    }
}

/// Open (or reuse) the hour reader for `key`.
fn reader_for<'s>(
    state: &'s mut WorkerState,
    key: &HourKey,
) -> rw_store::RwResult<&'s HourReader> {
    let cached = matches!(&state.hour, Some((have, _)) if have == key);
    if !cached {
        let reader = state.view.open_hour(&key.model, &key.run, key.hour)?;
        state.hour = Some((key.clone(), reader));
    }
    Ok(&state.hour.as_ref().expect("just cached").1)
}

fn hour_vars(state: &mut WorkerState, key: &HourKey) -> rw_store::RwResult<Vec<VarInfo>> {
    let reader = reader_for(state, key)?;
    Ok(reader
        .meta()
        .variables
        .iter()
        .map(|var| VarInfo {
            name: var.name.clone(),
            units: var.units.clone(),
            kind: if var.kind == "pressure3d" {
                VarKind::Pressure3D
            } else {
                VarKind::Surface2D
            },
            levels_hpa: var.levels_hpa.clone(),
        })
        .collect())
}

fn load_field(state: &mut WorkerState, key: &FieldKey) -> rw_store::RwResult<FieldData> {
    let reader = reader_for(state, &key.hour)?;
    let meta = reader.meta();
    let (nx, ny) = (meta.nx, meta.ny);
    let units = reader
        .variable(&key.var)
        .map(|var| var.units.clone())
        .unwrap_or_default();
    let values = reader.read_full_2d(&key.var)?;
    let range = finite_min_max(&values);
    Ok(FieldData {
        key: key.clone(),
        units,
        nx,
        ny,
        values,
        range,
    })
}

fn load_sounding(
    state: &mut WorkerState,
    key: &HourKey,
    fx: f64,
    fy: f64,
) -> rw_store::RwResult<SoundingData> {
    // Grid first (separate borrow scope from the hour reader).
    let run_id = (key.model.clone(), key.run.clone());
    let grid_cached = matches!(&state.grid, Some((have, _)) if *have == run_id);
    if !grid_cached {
        // The grid file is a nicety (lat/lon labels); profiles still work
        // without it, so failures are tolerated here.
        match state.view.open_grid(&key.model, &key.run) {
            Ok(grid) => state.grid = Some((run_id.clone(), grid)),
            Err(_) => state.grid = None,
        }
    }
    let (lat, lon) = match &state.grid {
        Some((_, grid)) => {
            let ix = (fx.round().max(0.0) as usize).min(grid.nx - 1);
            let iy = (fy.round().max(0.0) as usize).min(grid.ny - 1);
            let idx = iy * grid.nx + ix;
            (Some(grid.lat[idx]), Some(grid.lon[idx]))
        }
        None => (None, None),
    };

    let reader = reader_for(state, key)?;
    let vars_3d: Vec<(String, String, Vec<u16>)> = reader
        .meta()
        .variables
        .iter()
        .filter(|var| var.kind == "pressure3d")
        .map(|var| (var.name.clone(), var.units.clone(), var.levels_hpa.clone()))
        .collect();
    let mut vars = Vec::with_capacity(vars_3d.len());
    for (name, units, levels_hpa) in vars_3d {
        let values = reader.read_profile_3d(&name, fx, fy)?;
        vars.push(ProfileVar {
            name,
            units,
            levels_hpa,
            values,
        });
    }
    Ok(SoundingData {
        hour: key.clone(),
        fx,
        fy,
        lat,
        lon,
        vars,
    })
}
