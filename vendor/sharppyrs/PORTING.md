# Full SPC window port — progress tracker

Goal: port the ENTIRE SHARPpy-Reimagined sounding window to Rust/egui, matching
`scratchpad/reference_render.png` (Python render of
`examples/soundings/hrrr_point_36.68N_95.66W_f018.npz`, same data as
`testdata/hrrr_example.rs`).

Python sources: vendored SHARPpy 1.4.0a5 wheel at
`<scratchpad>/sharppy-whl/sharppy/viz/*.py`, repo overlays at
`<scratchpad>/sharppy-src/sharpmod/viz/*.py`, derived params at
`<scratchpad>/sharppy-src/sharpmod/sharptab/{derived,ecape}.py`.
(`<scratchpad>` = the local session scratchpad directory; not checked in.)
Reference render regen: `python -m sharpmod.render <npz> out.png` (deps installed --user).

## Window layout (from vendored SPCWidget + sharpmod mount_products)

- main grid: col 0 = SkewT (rows 0-2); col 1 = brand label over `grid2`; row 3
  spanning both = `grid3` bottom band.
- grid2 (upper right, 11 rows x 29 cols): speed_vs_height (0,0,11,3),
  inferred_temp_advection (0,3,11,2), hodo (0,5,8,24), storm_slinky (8,5,3,6),
  thetae_vs_pressure (8,11,3,6), srwinds_vs_height (8,17,3,6),
  hazard "Psbl Haz. Type" (8,23,3,6) [sharpmod hazard.py replaces watch_type].
  Hodo locator map inset overlays hodo top-left (sharpmod hodo_locator.py).
- grid3 (bottom band): IndexBoard spanning cols 0-2 (stretch 4,4,4),
  streamwiseness col 3, Effective Layer STP col 4 (stretch 7).
  (SHIP box-whisker inset is part of IndexBoard? -> verify in index_board.py)

## Checklist

### Architecture change (committed): numerics now come from the `sharprs` git
### dependency (FahrenheitResearch/sharprs); sharppyrs is rendering-only.
### extras.rs holds parcel-based Bunkers + SHARPpy-style FCST parcel.
### DerivedParams (src/derived.rs) is the interface for all table/inset values;
### golden_full.json is the reference. SoundingView (src/window.rs) composes
### the full window; tests/snapshot_full.rs renders it headlessly.
### Five background agents are porting: derived.rs impl, hodo(+map), 5 strips
### +hazard, index_board+ship, streamwiseness+stp. Integrate + iterate visually
### when they land.

### Numerics
- [x] skew-T numerics via sharprs (committed)
- [ ] parcelTraj (storm slinky trajectory)
- [ ] precip_water, k_index, t_totals, convective_temp, mean_relh, max_temp readout
- [ ] wndg, tei, esp, mmp, mburst, dcp, sig_severe, ship, stp_cin, stp_fixed,
      scp, ehi, sweat, thetae_diff, critical_angle, corfidi_mcs_motion,
      mbe_vectors, srh 1/3km, sr_wind, bulk_rich (BRN shear), inferred_temp_adv
- [ ] sharpmod derived.py params (6CAPE, 3CAPE, HGZ CAPE, NCAPE, ECAPE, WBZ hgt,
      SFC-500m/1km/3km LR, MOSHE, LSCP, Peskov, MCS index, LRGHAIL, NSTP,
      streamwiseness profile, hazard type) + ecape.py
- [ ] golden_full.json + tests

### Panels (src/panels/*.rs)
- [ ] speed strip (sharppy/viz/speed.py)
- [ ] advection strip (sharppy/viz/advection.py)
- [ ] hodograph (sharppy/viz/hodo.py) + locator map (sharpmod hodo_locator.py)
- [ ] storm slinky (sharppy/viz/slinky.py)
- [ ] thetae vs pres (sharppy/viz/thetae.py)
- [ ] SR wind vs height (sharppy/viz/srwinds.py)
- [ ] Psbl Haz Type (sharpmod hazard.py)
- [ ] IndexBoard 3 columns (sharpmod index_board.py + unit_text.py)
- [ ] SHIP inset (sharpmod ship.py)
- [ ] streamwiseness (sharpmod streamwiseness.py)
- [ ] Effective Layer STP (sharppy/viz/stp.py + STP_LABEL_SCALE etc from render.py)
- [ ] full-window composite widget `SoundingView` with grid layout + brand text

### Verification
- [x] skew-T snapshot vs reference (iteration 3)
- [ ] full-window snapshot vs reference_render.png
