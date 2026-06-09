# rusty-weather Plan 1: Workspace Bootstrap + Crate Extraction

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port rustwx's proven fast path into the new `rusty-weather` workspace so it compiles clean, passes the ported test suites, and renders real HRRR map PNGs end to end — with all out-of-scope code (radar, satellite, WRF, lightning, wxstore, agent platform, AIFS/Earth2) left behind.

**Architecture:** Mechanical extraction from the read-only worktree at `C:\Users\drew\rustwx-fastplots-wt` (branch `review/grib-wxa-fast-plots-20260605`). Nine workspace crates + seven vendored crates copy over with names unchanged; pruning happens at module boundaries (`rustwx-products`, `rustwx-io`) guided by an explicit drop list plus the compiler. Two smoke binaries adapted from rustwx's proven `direct_batch`/`derived_batch` drivers prove the extraction renders identical plots. The new store, scheduler/daemon, and web UI are **later plans** — nothing in this plan builds them.

**Tech Stack:** Rust (edition 2024, rust-version 1.85), cargo workspace, existing deps only (image, rayon, serde, zstd, ureq/rustls, shapefile, rusttype). No new dependencies in this plan.

**Spec:** `docs/superpowers/specs/2026-06-09-rusty-weather-design.md`

---

## Ground rules for every task

- **Source of truth:** `C:\Users\drew\rustwx-fastplots-wt` — read-only. Never edit files there.
- **Destination:** `C:\Users\drew\rusty-weather` — all work happens here. Shell commands below are PowerShell and assume this as the working directory.
- **Copy method:** `Copy-Item -Recurse` (the worktree is a fresh checkout; there are no `target/` dirs to exclude).
- **No stubs, ever.** Never satisfy the compiler with `todo!()`, `unimplemented!()`, empty fn bodies, or commented-out code. Deletion or honest port only.
- **Rule R1 — slop-reference removal.** When `cargo check` fails on a reference to dropped code:
  1. If the reference lives in a file on the drop list → the file should already be deleted; delete the stale reference instead.
  2. If a KEPT file references dropped code AND the referencing item (fn/struct/match-arm/CLI arg) exists *only* to serve dropped functionality (its name/docs mention satellite, radar, lightning, wxstore, earth2, AIFS, mesoanalysis, native_dataset, intelligence, agent, wxmod) → delete that item entirely, including its callers if they too are slop-only.
  3. If a KEPT file references dropped code from a load-bearing path (the direct/derived/gridded render flow needs it) → **STOP. Do not improvise.** Report the file/line and wait for review; the module may need to move to the keep list.
  4. After each fix round re-run `cargo check`; when green, grep for the dropped crate/module names to confirm zero references remain.
  5. List every R1 deletion (file + item name) in the body of that task's commit message so review can audit.
- **Commit cadence:** every task ends in a commit; intermediate commits are welcome. Messages end with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- **First build is slow** (downloads + ~150K lines). `cargo check` of the full workspace can take several minutes; that is normal.

## Final file structure (what exists when this plan is done)

```
rusty-weather/
  Cargo.toml                    # workspace manifest (Task 1)
  Cargo.lock                    # committed (app workspace)
  .gitignore                    # Task 1
  README.md                     # Task 1 stub, finalized Task 12
  assets/basemap/               # Natural Earth + counties shapefiles (Task 6)
  vendor/                       # 8 crates, copied byte-for-byte (Task 2; wx-radar rides along as metrust's dep)
    wx-core/  wx-math/  wx-field/  wx-radar/  grib-core/  metrust/  ecape-rs/  sharprs/
  crates/
    rustwx-core/                # Task 3 — domain types
    rustwx-contour/             # Task 3 — contour topology
    rustwx-regrid/              # Task 3 — regridding
    rustwx-models/              # Task 4 — model registry (catalog pruned to 6 in Task 11)
    rustwx-io/                  # Task 5 — GRIB fetch/extract (earth2_archive removed)
    rustwx-render/              # Task 6 — map rendering (cuda feature stripped)
    rustwx-calc/                # Task 7 — diagnostics (cuda feature stripped)
    rustwx-sounding/            # Task 7 — sounding rendering
    rustwx-products/            # Task 8 — pruned: 24 modules dropped
    rusty-weather/              # Task 9 — bin crate: smoke_direct, smoke_derived
  docs/superpowers/specs/       # already present
  docs/superpowers/plans/       # this file
  out/                          # smoke output, gitignored
```

---

### Task 1: Workspace scaffold

**Files:**
- Create: `Cargo.toml`, `.gitignore`, `README.md`

- [x] **Step 1: Write the workspace manifest**

Create `Cargo.toml` with exactly:

```toml
[workspace]
members = []
exclude = [
    "vendor/ecape-rs",
    "vendor/grib-core",
    "vendor/metrust",
    "vendor/sharprs",
    "vendor/wx-core",
    "vendor/wx-field",
    "vendor/wx-math",
]
resolver = "2"

[workspace.package]
edition = "2024"
license = "MIT"
publish = false
rust-version = "1.85"

[workspace.dependencies]
clap = { version = "4.5", features = ["derive"] }
rayon = "1"
rustls = { version = "0.23", default-features = false, features = ["std"] }
rustls-rustcrypto = "0.0.2-alpha"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
ureq = { version = "3", default-features = false, features = ["rustls-no-provider", "rustls-webpki-roots"] }

[profile.release]
lto = "thin"
codegen-units = 1
```

(Mirrors rustwx's manifest minus the dropped crates and minus the `hdf5-reader` patch — nothing in the keep-set uses it once Tasks 5 and 8 land. `members` fills in as crates arrive.)

- [x] **Step 2: Write `.gitignore`**

```gitignore
/target
/out
/store
*.tmp
```

- [x] **Step 3: Write `README.md` stub**

```markdown
# rusty-weather

A self-contained weather model viewer: fetch HRRR / GFS / RRFS-A / REFS / NBM / RAP,
store hours in a fast-access format, and serve map plots + instant soundings on a
local webpage. Full Rust. Extracted from the rustwx fast path.

Design: docs/superpowers/specs/2026-06-09-rusty-weather-design.md
Status: extraction in progress (Plan 1).
```

- [x] **Step 4: Commit**

```powershell
git add -A; git commit -m "chore: scaffold rusty-weather workspace

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: Vendored crates

**Files:**
- Create: `vendor/wx-core/`, `vendor/wx-math/`, `vendor/wx-field/`, `vendor/grib-core/`, `vendor/metrust/`, `vendor/ecape-rs/`, `vendor/sharprs/` (byte-for-byte copies)

- [x] **Step 1: Copy the seven vendor crates**

```powershell
$src = 'C:\Users\drew\rustwx-fastplots-wt\vendor'
New-Item -ItemType Directory -Force vendor | Out-Null
foreach ($c in 'wx-core','wx-math','wx-field','wx-radar','grib-core','metrust','ecape-rs','sharprs') {
  Copy-Item -Recurse "$src\$c" "vendor\$c"
}
```

Do NOT copy `netcrust` (WRF/NetCDF-only — out of scope). `wx-radar` rides along ONLY because `metrust` path-depends on it unconditionally (re-exports in its `io`/`plots` modules); copying it preserves metrust byte-for-byte. Nothing else may depend on wx-radar. *(Amended during execution — original plan excluded wx-radar.)*

Add `"vendor/wx-radar",` to the workspace `exclude` list in the root `Cargo.toml`.

- [x] **Step 2: Check each vendor crate compiles standalone (except sharprs — deferred)**

```powershell
foreach ($c in 'wx-core','wx-math','wx-field','wx-radar','grib-core','metrust','ecape-rs') {
  cargo check --manifest-path "vendor\$c\Cargo.toml"; if (-not $?) { Write-Error "FAILED: $c"; break }
}
```

Expected: each finishes with `Finished` and no errors. **sharprs is NOT checked here:** its `src/render/canvas.rs` does `include_bytes!("../../../../crates/rustwx-render/assets/fonts/SourceSans3-Regular.ttf")`, which only resolves after Task 6 copies the render crate. Its standalone check moves to Task 7 Step 3 (alongside rustwx-sounding). *(Amended during execution.)*

- [x] **Step 3: Commit**

```powershell
git add -A; git commit -m "feat: vendor wx-core, wx-math, wx-field, grib-core, metrust, ecape-rs, sharprs

Byte-for-byte from rustwx review/grib-wxa-fast-plots-20260605.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: Leaf crates — rustwx-core, rustwx-contour, rustwx-regrid

**Files:**
- Create: `crates/rustwx-core/`, `crates/rustwx-contour/`, `crates/rustwx-regrid/`
- Modify: `Cargo.toml` (members)

- [x] **Step 1: Copy the three crates**

```powershell
$src = 'C:\Users\drew\rustwx-fastplots-wt\crates'
New-Item -ItemType Directory -Force crates | Out-Null
foreach ($c in 'rustwx-core','rustwx-contour','rustwx-regrid') { Copy-Item -Recurse "$src\$c" "crates\$c" }
```

- [x] **Step 2: Add them to workspace members**

In `Cargo.toml`, set:

```toml
members = [
    "crates/rustwx-contour",
    "crates/rustwx-core",
    "crates/rustwx-regrid",
]
```

- [x] **Step 3: Run their tests**

```powershell
cargo test -p rustwx-core -p rustwx-contour -p rustwx-regrid
```

Expected: PASS (these are leaf crates depending only on vendor crates and each other). Any failure here means the copy is bad — re-copy, don't patch.

- [x] **Step 4: Commit**

```powershell
git add -A; git commit -m "feat: port rustwx-core, rustwx-contour, rustwx-regrid

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: rustwx-models (whole, catalog pruned later in Task 11)

**Files:**
- Create: `crates/rustwx-models/`
- Modify: `Cargo.toml` (members)

- [x] **Step 1: Copy, add member**

```powershell
Copy-Item -Recurse 'C:\Users\drew\rustwx-fastplots-wt\crates\rustwx-models' 'crates\rustwx-models'
```

Add `"crates/rustwx-models",` to `members` (keep the list alphabetized).

- [x] **Step 2: Test**

```powershell
cargo test -p rustwx-models
```

Expected: PASS. Note: the crate still registers all ~18 models at this point. That is deliberate — `ModelId` match arms thread through `rustwx-products`, so model pruning is deferred to Task 11 (catalog surface) after the smoke proves the extraction. Do not prune anything here.

- [x] **Step 3: Commit**

```powershell
git add -A; git commit -m "feat: port rustwx-models (full registry; catalog prune deferred)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: rustwx-io minus the Earth2/AIFS archive reader

**Files:**
- Create: `crates/rustwx-io/` (minus `src/earth2_archive.rs`)
- Modify: `crates/rustwx-io/Cargo.toml`, `crates/rustwx-io/src/lib.rs`, workspace `Cargo.toml` (members)

- [x] **Step 1: Copy and delete the Earth2 module**

```powershell
Copy-Item -Recurse 'C:\Users\drew\rustwx-fastplots-wt\crates\rustwx-io' 'crates\rustwx-io'
Remove-Item 'crates\rustwx-io\src\earth2_archive.rs'
```

`earth2_archive.rs` is the ONLY user of `netcrust`/`hdf5-reader` in this crate (verified by grep) and serves AIFS local archives — out of scope.

- [x] **Step 2: Cut the dropped deps and the wrf feature from `crates/rustwx-io/Cargo.toml`**

Delete these lines:

```toml
hdf5-reader = "0.3"
netcrust = { path = "../../vendor/netcrust" }
rustwx-wrf = { path = "../rustwx-wrf", optional = true }
```

and delete the feature line `wrf = ["dep:rustwx-wrf"]` (keep `default = []`).

- [x] **Step 3: Remove the module declaration and add the member**

In `crates/rustwx-io/src/lib.rs`, delete the `pub mod earth2_archive;` line. Add `"crates/rustwx-io",` to workspace members.

- [x] **Step 4: Delete the wrf-gated blocks, then compile and apply Rule R1**

First find and delete every `#[cfg(feature = "wrf")]` item whole (the feature no longer exists; leftover gates would dangle as `unexpected_cfgs` warnings):

```powershell
Select-String -Path 'crates\rustwx-io\src\*.rs' -Pattern 'feature = "wrf"' | Select-Object Path,LineNumber
```

Then:

```powershell
cargo check -p rustwx-io
```

Expected: errors pointing at remaining `earth2_archive::` references inside `lib.rs` (AIFS source-routing branches). Apply R1: delete those branches/items — they are AIFS/WRF-only by name. STOP per R1.3 if anything load-bearing (HRRR/GFS GRIB path) references them. Re-run until green, then verify:

```powershell
Select-String -Path 'crates\rustwx-io\src\*.rs' -Pattern 'earth2|netcrust|hdf5|rustwx_wrf' -CaseSensitive:$false
```

Expected: zero matches (comments mentioning them are fine to delete too).

- [x] **Step 5: Test and commit**

```powershell
cargo test -p rustwx-io
```

Expected: PASS (tests referencing earth2 get deleted under R1 and listed in the commit body).

```powershell
git add -A; git commit -m "feat: port rustwx-io without Earth2/AIFS archive support

R1 deletions: <list each removed fn/test here>

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: rustwx-render + basemap assets

**Files:**
- Create: `crates/rustwx-render/`, `assets/basemap/`
- Modify: `crates/rustwx-render/Cargo.toml`, workspace `Cargo.toml` (members)

- [x] **Step 1: Copy assets first, then the crate**

```powershell
New-Item -ItemType Directory -Force assets | Out-Null
Copy-Item -Recurse 'C:\Users\drew\rustwx-fastplots-wt\assets\basemap' 'assets\basemap'
Copy-Item -Recurse 'C:\Users\drew\rustwx-fastplots-wt\crates\rustwx-render' 'crates\rustwx-render'
```

Basemap resolution in `features.rs` searches `<workspace_root>/assets/basemap` (with an env-var override), so the same relative layout preserves behavior. Shapefiles are binary — confirm sizes match the source after copy:

```powershell
(Get-ChildItem -Recurse assets\basemap | Measure-Object Length -Sum).Sum
(Get-ChildItem -Recurse C:\Users\drew\rustwx-fastplots-wt\assets\basemap | Measure-Object Length -Sum).Sum
```

Expected: identical byte totals.

- [x] **Step 2: Strip the cuda feature**

Open `crates/rustwx-render/Cargo.toml`. Delete the `cuda = [...]` feature line and any optional dependency it enables (a path dep pointing outside the keep-set, e.g. `rustwx-cuda`). Then find and delete the gated code:

```powershell
Select-String -Path 'crates\rustwx-render\src\*.rs' -Pattern 'feature = "cuda"' -List
```

For each hit, delete the whole `#[cfg(feature = "cuda")]` item (the CPU fallback is the unconditional path and stays). Do NOT touch any non-cfg-gated code.

- [x] **Step 3: Add member, run tests including the verify lane**

Add `"crates/rustwx-render",` to members.

```powershell
cargo test -p rustwx-render
Get-ChildItem 'crates\rustwx-render\verify' -ErrorAction SilentlyContinue
```

Expected: tests PASS. If a `verify/` directory exists with its own runner, note how it's invoked (check for a README or bin target inside) and run it; record the result in the commit message.

- [x] **Step 4: Commit**

```powershell
git add -A; git commit -m "feat: port rustwx-render (cuda feature stripped) + basemap assets

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: rustwx-calc and rustwx-sounding

**Files:**
- Create: `crates/rustwx-calc/`, `crates/rustwx-sounding/`
- Modify: both crates' `Cargo.toml` if cuda-gated, workspace `Cargo.toml` (members)

- [x] **Step 1: Copy both crates, add members**

```powershell
foreach ($c in 'rustwx-calc','rustwx-sounding') {
  Copy-Item -Recurse "C:\Users\drew\rustwx-fastplots-wt\crates\$c" "crates\$c"
}
```

Add `"crates/rustwx-calc",` and `"crates/rustwx-sounding",` to members.

- [x] **Step 2: Strip cuda from rustwx-calc (same procedure as Task 6 Step 2)**

```powershell
Select-String -Path 'crates\rustwx-calc\src\*.rs' -Pattern 'feature = "cuda"' -List
```

Delete the feature line, the optional dep, and each gated item. CPU path stays untouched.

- [x] **Step 3: Test**

```powershell
cargo test -p rustwx-calc -p rustwx-sounding
```

Expected: PASS. rustwx-calc exercises vendored metrust/ecape-rs; rustwx-sounding exercises vendored sharprs (its Cargo.toml already enables sharprs's render feature — copied as-is).

- [x] **Step 4: Commit**

```powershell
git add -A; git commit -m "feat: port rustwx-calc (cuda stripped) and rustwx-sounding

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: rustwx-products, pruned

This is the heart of the extraction. 24 modules drop; the direct/derived/gridded render path stays.

**Files:**
- Create: `crates/rustwx-products/` (pruned)
- Modify: `crates/rustwx-products/Cargo.toml`, `crates/rustwx-products/src/lib.rs`, workspace `Cargo.toml` (members)

- [x] **Step 1: Copy, then delete the drop-list modules**

```powershell
Copy-Item -Recurse 'C:\Users\drew\rustwx-fastplots-wt\crates\rustwx-products' 'crates\rustwx-products'
$drop = @(
  'agent_backend','artifact_bundle','comparison','cross_section','custom_poi',
  'dataset_export','gallery','grib_ensemble','intelligence','lightning',
  'mesoanalysis','mesoanalysis_calibration','native_dataset','native_dataset_hrrr',
  'native_dataset_materializer','native_dataset_obs','native_dataset_shard_store',
  'orchestrator','publication_provenance','satellite','volume_store',
  'wxstore_export','wxstore_profile','wxstore_wxa'
)
foreach ($m in $drop) {
  Remove-Item -Recurse -Force -ErrorAction SilentlyContinue "crates\rustwx-products\src\$m.rs","crates\rustwx-products\src\$m"
}
```

(Some names are single files, some are directories with a same-named `.rs`; the loop removes both forms.)

- [x] **Step 2: Rewrite the module list in `src/lib.rs`**

Replace the block of `pub mod` declarations so that exactly these remain (preserving original order and any doc comments between them):

```rust
pub mod cache;
pub mod catalog;
pub mod derived;
pub mod direct;
pub mod ecape;
pub mod gridded;
pub mod heavy;
pub mod hrrr;
pub mod named_geometry;
pub mod non_ecape;
pub mod places;
pub mod planner;
pub mod plot_design;
pub mod point_timeseries;
pub mod publication;
pub(crate) mod qpf;
pub mod runtime;
pub mod sampling;
pub mod severe;
pub mod shared_context;
pub mod source;
pub mod spec;
pub mod thermo_native;
pub mod windowed;
pub mod windowed_decoder;
```

(Keep the existing `mod tests;` declaration near the bottom of the file.) Also delete any `pub use` re-exports in `lib.rs` that point at dropped modules.

**Keep rationale, so review can sanity-check:** `direct`/`derived`/`gridded`/`hrrr`/`heavy`/`severe`/`ecape`/`non_ecape`/`qpf`/`thermo_native` are the product science + render flows; `places` is the city-label overlay used by direct plots (part of the dialed-in look); `publication` provides atomic writes + run manifests used by the batch drivers; `cache`/`source`/`spec`/`shared_context`/`plot_design`/`windowed`/`windowed_decoder`/`sampling`/`named_geometry`/`catalog`/`planner`/`runtime`/`point_timeseries` are shared infrastructure for those flows. If compilation proves any of the last group is *only* referenced by dropped modules, delete it too under R1 and note it in the commit.

- [x] **Step 3: Cut dropped deps and the wrf feature from `crates/rustwx-products/Cargo.toml`**

Delete these dependency lines:

```toml
rustwx-cross-section = { path = "../rustwx-cross-section" }
rustwx-radar = { path = "../rustwx-radar" }
rustwx-wrf = { path = "../rustwx-wrf", optional = true }
hdf5-reader = "0.3"
netcrust = { path = "../../vendor/netcrust" }
```

and the feature line `wrf = ["dep:rustwx-wrf", "rustwx-io/wrf"]`.

- [x] **Step 4: Delete the wrf-gated blocks in kept modules**

The two known sites (verified by grep): `derived.rs:51` (`use rustwx_wrf::{WrfFile, looks_like_wrf};`) and `gridded.rs:27` (`use rustwx_wrf as wrf;`). Find every gated region and delete it whole:

```powershell
Select-String -Path 'crates\rustwx-products\src\*.rs' -Pattern 'feature = "wrf"' | Select-Object Path,LineNumber
```

Each hit is a `#[cfg(feature = "wrf")]` item (fn, use, match arm, or block) — delete the entire item, not just the attribute. If a WRF reference appears WITHOUT a cfg gate, treat it under R1.2 (WRF-only by name → delete the item).

- [x] **Step 5: Add member, compile, apply Rule R1 to convergence**

Add `"crates/rustwx-products",` to members.

```powershell
cargo check -p rustwx-products
```

Expected: a burst of unresolved-import errors in kept modules that reference dropped ones (e.g. `runtime`/`planner`/`catalog` enumerating satellite or native-dataset products, `tests` covering dropped modules). Apply R1 iteratively. Likely shapes:
- `use crate::satellite::...` in a kept module → the using item serves satellite output → delete the item (R1.2).
- A product-catalog table listing dropped product families → delete those entries/variants only.
- A kept module that turns out to be agent-platform glue (imports `intelligence`/`orchestrator` everywhere) → move it to the drop list, delete it, remove its `pub mod` line, and note the promotion in the commit.
- Anything where deletion would sever the direct/derived/gridded flow → STOP and report (R1.3).

When green, verify no stragglers:

```powershell
Select-String -Path 'crates\rustwx-products\src' -Pattern 'rustwx_radar|netcrust|hdf5_reader|rustwx_cross_section|rustwx_wrf|wxstore|volume_store|satellite|mesoanalysis|lightning|earth2' -List
```

Expected: zero load-bearing matches (doc-comment mentions: delete them too).

- [x] **Step 6: Test and commit**

```powershell
cargo test -p rustwx-products
```

Expected: PASS after R1 removes tests of dropped modules.

```powershell
git add -A; git commit -m "feat: port rustwx-products pruned to the map/sounding fast path

Dropped modules: agent_backend artifact_bundle comparison cross_section custom_poi
dataset_export gallery grib_ensemble intelligence lightning mesoanalysis
mesoanalysis_calibration native_dataset* orchestrator publication_provenance
satellite volume_store wxstore_*

R1 deletions: <list each removed item here>

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 9: Smoke binaries (`rusty-weather` bin crate)

Adapt rustwx's two proven batch drivers verbatim-minus-slop. These are scaffolding for validating the extraction; the real CLI (`serve`/`fetch`/`render`) arrives with the daemon plan and will replace them.

**Files:**
- Create: `crates/rusty-weather/Cargo.toml`, `crates/rusty-weather/src/main.rs`, `crates/rusty-weather/src/contour_mode.rs`, `crates/rusty-weather/src/domain.rs`, `crates/rusty-weather/src/region.rs`, `crates/rusty-weather/src/bin/smoke_direct.rs`, `crates/rusty-weather/src/bin/smoke_derived.rs`
- Modify: workspace `Cargo.toml` (members)

- [x] **Step 1: Create the crate manifest**

`crates/rusty-weather/Cargo.toml`:

```toml
[package]
name = "rusty-weather"
version = "0.1.0"
edition.workspace = true
license.workspace = true
publish.workspace = true
rust-version.workspace = true
default-run = "rusty-weather"

[dependencies]
anyhow = "1"
chrono = { version = "0.4", default-features = false, features = ["std"] }
clap.workspace = true
image = { version = "0.25", default-features = false, features = ["png"] }
rayon.workspace = true
serde.workspace = true
serde_json.workspace = true
rustwx-calc = { path = "../rustwx-calc" }
rustwx-core = { path = "../rustwx-core" }
rustwx-io = { path = "../rustwx-io" }
rustwx-models = { path = "../rustwx-models" }
rustwx-products = { path = "../rustwx-products" }
rustwx-render = { path = "../rustwx-render" }
rustwx-sounding = { path = "../rustwx-sounding" }
```

- [x] **Step 2: Create a placeholder main**

`crates/rusty-weather/src/main.rs`:

```rust
fn main() {
    eprintln!(
        "rusty-weather {}: daemon not built yet (see docs/superpowers/specs/). \
         Use the smoke_direct / smoke_derived binaries to validate the extraction.",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2);
}
```

- [x] **Step 3: Copy the shared CLI helper modules and the two drivers**

```powershell
$cli = 'C:\Users\drew\rustwx-fastplots-wt\crates\rustwx-cli\src'
foreach ($f in 'contour_mode.rs','domain.rs','region.rs') { Copy-Item "$cli\$f" "crates\rusty-weather\src\$f" }
New-Item -ItemType Directory -Force 'crates\rusty-weather\src\bin' | Out-Null
Copy-Item "$cli\bin\direct_batch.rs"  'crates\rusty-weather\src\bin\smoke_direct.rs'
Copy-Item "$cli\bin\derived_batch.rs" 'crates\rusty-weather\src\bin\smoke_derived.rs'
```

The `#[path = "../contour_mode.rs"]` attributes inside the bin files resolve to `src/` from `src/bin/` — the copied layout matches, no edits needed for those.

- [x] **Step 4: Trim the Earth2/AIFS surface from both smoke bins**

In `smoke_direct.rs`, known Earth2 items (from the source read): the import `use rustwx_io::earth2_archive::{Earth2EnsembleSelector, Earth2EnsembleStat};` (line 18), the `Earth2StatArg` enum + its `From` impl (lines 30–53), and every CLI arg/branch mentioning `earth2`. Find them all in both files:

```powershell
Select-String -Path 'crates\rusty-weather\src\bin\*.rs','crates\rusty-weather\src\*.rs' -Pattern 'earth2' -CaseSensitive:$false
```

Delete each item whole (R1.2 — AIFS-only by name). Also change the hardcoded default output dir in both bins from `C:\\Users\\drew\\rustwx\\proof` to `out`.

- [x] **Step 5: Build, applying R1 to any further driver-level slop references**

Add `"crates/rusty-weather",` to members, then:

```powershell
cargo build --release -p rusty-weather
```

Expected: compiles after Earth2 trims. If the drivers reference other dropped products modules (e.g. a `--publish` path using provenance), R1 them. STOP per R1.3 if the core direct/derived flow breaks.

- [x] **Step 6: Commit**

```powershell
git add -A; git commit -m "feat: add smoke_direct and smoke_derived extraction-validation binaries

Adapted from rustwx direct_batch/derived_batch; Earth2/AIFS surface removed.
R1 deletions: <list>

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 10: Live smoke validation (the extraction acceptance test)

Requires network (AWS/NOMADS reachable). Use yesterday's 00z HRRR run so availability is guaranteed regardless of time of day.

- [x] **Step 1: Render direct products for one HRRR hour**

```powershell
$d = (Get-Date).AddDays(-1).ToString('yyyyMMdd')
cargo run --release -p rusty-weather --bin smoke_direct -- --model hrrr --date $d --cycle 0 --forecast-hour 6 --region midwest --all-supported --out-dir out\smoke_direct
```

Expected: completes in a few minutes (download dominates); no panic; exit 0.

- [x] **Step 2: Verify the output is real**

```powershell
Get-ChildItem -Recurse out\smoke_direct -Filter *.png | Measure-Object | Select-Object Count
Get-ChildItem -Recurse out\smoke_direct -Filter *.png | Sort-Object Length | Select-Object -First 3 Name,Length
```

Expected: Count ≥ 10; smallest PNG > 50 KB (a blank/failed render is tiny). Open 2–3 PNGs (e.g. with `Invoke-Item`) and confirm: state/county basemap lines present, colorbar present, plot style matches rustwx output (the operator knows the dialed-in look on sight — flag for their eyeball).

- [x] **Step 3: Render derived products for the same hour**

```powershell
cargo run --release -p rusty-weather --bin smoke_derived -- --model hrrr --date $d --cycle 0 --forecast-hour 6 --region midwest --out-dir out\smoke_derived
```

(If `smoke_derived` requires explicit `--recipe` values, list them with `--help` and pick 3–4 severe/CAPE recipes.) Expected: PNGs with filled contour fields, no panic.

- [x] **Step 4: Run the full workspace test suite one more time**

```powershell
cargo test --workspace
```

Expected: PASS across all 10 crates.

- [x] **Step 5: Commit any fixes; record timings**

Note wall-clock for fetch+extract and for render in the commit message (these are the Plan-1 baseline for the spec's perf table).

```powershell
git add -A; git commit -m "test: live HRRR smoke validation of the extraction

smoke_direct: <N> PNGs, fetch+extract <X>s, render <Y>s
smoke_derived: <N> PNGs

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 11: Prune the model catalog surface to the six target models

The `ModelId` enum and its match arms stay (cheap, and threaded through products). What gets cut is the *enumeration surface* — the list the future server/CLI iterates to present models.

**Files:**
- Modify: `crates/rustwx-models/src/lib.rs` (catalog/summary list only)

- [x] **Step 1: Locate the enumeration surface**

```powershell
Select-String -Path 'crates\rustwx-models\src\lib.rs' -Pattern 'fn (all_models|model_summaries|supported_models)|const (ALL_MODELS|MODEL)' | Select-Object LineNumber,Line
```

Identify the function/const that yields the full model list (the one `model_summary` lookups iterate, used by CLIs to enumerate).

- [x] **Step 2: Write a failing test for the new surface**

Add to the models test module:

```rust
#[test]
fn catalog_exposes_exactly_the_six_supported_models() {
    use rustwx_core::ModelId;
    let expected = [
        ModelId::Hrrr,
        ModelId::Gfs,
        ModelId::RrfsA,
        ModelId::Refs,
        ModelId::Nbm,
        ModelId::Rap,
    ];
    let got = crate::supported_models();
    assert_eq!(got, expected);
}
```

(Adjust variant names to the actual `ModelId` spelling found in `rustwx-core` — check with `Select-String -Path 'crates\rustwx-core\src\*.rs' -Pattern 'pub enum ModelId' -Context 0,25`. If REFS is not a distinct `ModelId` variant — it may be an ensemble mode of RRFS — note that in the commit and test for the five that exist; REFS plumbing is a later-plan concern.)

- [x] **Step 3: Run it to verify it fails**

```powershell
cargo test -p rustwx-models catalog_exposes_exactly
```

Expected: FAIL — `supported_models` doesn't exist yet.

- [x] **Step 4: Implement `supported_models()`**

In `crates/rustwx-models/src/lib.rs`, next to the existing full-list function:

```rust
/// The models rusty-weather exposes to users. The wider registry remains
/// linked (ModelId match arms thread through rustwx-products), but every
/// user-facing enumeration must go through this list.
pub fn supported_models() -> [rustwx_core::ModelId; 6] {
    use rustwx_core::ModelId;
    [
        ModelId::Hrrr,
        ModelId::Gfs,
        ModelId::RrfsA,
        ModelId::Refs,
        ModelId::Nbm,
        ModelId::Rap,
    ]
}
```

(Same variant-name caveat as Step 2.) Do NOT delete the other models' registry entries in this plan — that deep prune happens once the daemon exists and dead code is provable.

- [x] **Step 5: Run tests, commit**

```powershell
cargo test -p rustwx-models
git add -A; git commit -m "feat: add supported_models() catalog surface for the six target models

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 12: Finalize README, record the baseline, tag

**Files:**
- Modify: `README.md`, this plan file (check the boxes)

- [x] **Step 1: Update README with real build/run instructions**

Replace the stub's status line with:

```markdown
## Status

Extraction complete (Plan 1). The workspace builds and renders live HRRR plots:

    cargo run --release -p rusty-weather --bin smoke_direct -- --model hrrr --date YYYYMMDD --cycle 0 --forecast-hour 6 --region midwest --all-supported --out-dir out

Next: unified store (rw-store), then the serve daemon, then the web UI — see
docs/superpowers/specs/2026-06-09-rusty-weather-design.md.

## Layout

- `crates/` — ported rustwx crates (names kept for diffability) + the `rusty-weather` bin crate
- `vendor/` — vendored deps (sharprs, metrust, grib-core, wx-*, ecape-rs)
- `assets/basemap/` — Natural Earth + US county shapefiles
```

- [x] **Step 2: Check every box in this plan, commit, tag**

```powershell
git add -A; git commit -m "docs: finalize Plan 1 (extraction) README and plan checkboxes

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
git tag extraction-complete
```

---

## Follow-on plans (NOT in this plan — each gets its own writing-plans pass)

1. **Plan 2 — rw-store:** the unified format (per-hour `.rws`, windowed 2D tiles, column-chunked 3D), TDD'd against synthetic grids, then wired to ingest; includes building the committed GRIB fixture for CI smoke. Chunk-shape tuning uses Task 10's timing baseline.
2. **Plan 3 — pipeline + scheduler:** the global rayon pool, job model, `fetch`/`render` subcommands replacing the smoke bins, per-stage timing into `run.json`, build-hash stamping.
3. **Plan 4 — rw-server + frontend:** axum API + SSE + embedded static UI + click-for-sounding.
4. **Plan 5 — node validation:** deploy to node3/node4, same-cycle comparison vs rustwx, the 3-concurrent-models ≤1.5× acceptance test, deep-prune of dead model registry code.
