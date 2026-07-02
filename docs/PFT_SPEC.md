# Pyrocumulonimbus Firepower Threshold (PFT) — Implementation Specification

**Target:** Rust implementation against HRRR model soundings (pressure-level T, Td, u, v, Z + surface fields).
**Primary source:** Tory & Kepert (2021), *Weather and Forecasting* 36(2), 439–456, doi:10.1175/WAF-D-20-0027.1 (full text obtained and read; all equation numbers below refer to that paper unless noted).
**Secondary sources:** Tory, Thurston & Kepert (2018), *MWR* 146, 2579–2598 (SP-curve equations); Tory & Peace, BNHCRC Report No. 694.2021 ("real-time trial", PFT1/PFT2); reference implementation `rnleach/sounding-analysis` + `rnleach/metfor` (Rust, MIT-licensed, verified line-by-line against the paper).

**Verification status of this spec:** Every equation below was transcribed from the actual published papers (PDFs read in full), not reconstructed from memory. Where the paper is ambiguous and an implementation had to make a choice, the ambiguity is flagged in §10 with both options and a recommendation.

---

## 1. Concept

Instead of guessing the fire's heat/moisture contribution (the fireCAPE approach), the PFT inverts the problem: it computes the **minimum total sensible-heat flux (firepower, watts) entering the base of a smoke plume that is required for the plume to reach free moist convection deep enough to make a pyroCb**, given only an atmospheric column (T, q, wind). **Lower PFT = more favorable atmosphere for pyroCb.** Output is total firepower in gigawatts (GW), *not* per unit area.

The model has two parts:

1. **Below cloud (dry-plume layer):** a Briggs (1975, 1984) bent-over plume in a constant horizontal wind `U` and neutral stratification, with entrainment velocity proportional to plume ascent rate (`V_ent = β·w`, the classic Briggs closure). This yields an analytic relation between the heat flux at the plume base and the plume's excess potential temperature at any height.
2. **At/above cloud base:** the Tory et al. (2018) saturation-point (SP) curve + parcel theory. A thermodynamic search finds the *minimum-energy* moist path that can freely convect from plume condensation up to an electrification-level cloud top (−20 °C), giving the free-convection height `z_fc` and the required plume warmth `Δθ_fc`.

The two are joined at `z_fc` to give the analytic PFT (Eq. 25 below).

---

## 2. Inputs required from a model profile

### 2.1 Profile variables (all levels from the surface to at least the −20 °C level; in practice up to ≥ 300 hPa)

| Variable | Symbol | Units | HRRR source |
|---|---|---|---|
| Pressure | `P` | hPa | pressure-level coordinate (prs files, 25-hPa spacing) |
| Geopotential height | `z` | m ASL | `HGT` on pressure levels |
| Temperature | `T` | K | `TMP` on pressure levels |
| Dewpoint (or RH / specific humidity) | `Td` | K | `DPT` (or `RH`) on pressure levels |
| Wind components | `u, v` | m s⁻¹ | `UGRD`, `VGRD` on pressure levels |

### 2.2 Surface fields

| Variable | Symbol | HRRR source |
|---|---|---|
| Surface pressure | `P_sfc` | `PRES:surface` |
| Terrain elevation | `z_sfc` | `HGT:surface` |
| 2-m T, 2-m Td | | `TMP:2 m`, `DPT:2 m` |
| 10-m u, v | | `UGRD:10 m`, `VGRD:10 m` |

**Profile assembly rule:** discard pressure levels with `P > P_sfc` (below ground); prepend a surface level built from (`P_sfc`, `z_sfc`, T₂ₘ, Td₂ₘ, u₁₀ₘ, v₁₀ₘ). All heights below are converted to **height above ground level**: `h = z − z_sfc`. (The radiosonde examples in the paper adjust `z_fc` for the elevation difference between sounding site and fire; on a model grid the column's own surface is the fire surface, so no adjustment is needed.)

**Derived per level:**
- Potential temperature: `θ = T·(P0/P)^κ`, `P0 = 1000 hPa`, `κ = Rd/Cpd = 0.2854`.
- Specific humidity from dewpoint: `e = e_s(Td)` with a standard saturation vapor pressure formula (e.g. Bolton 1980: `e_s(T) = 6.112·exp(17.67·T_C/(T_C + 243.5))` hPa), then `q = ε·e / (P − (1−ε)·e)` (kg kg⁻¹), `ε = Rd/Rv = 0.622`.

### 2.3 Constants (Table 2 of Tory & Kepert 2021 — verbatim)

| Constant | Value | Meaning |
|---|---|---|
| `π` | 3.14159265 | circle constant |
| `Cpd` | 1005.7 J kg⁻¹ K⁻¹ | specific heat of dry air, const. pressure |
| `Rd` | 287.04 J kg⁻¹ K⁻¹ | gas constant, dry air (metfor uses 287.058; immaterial) |
| `κ = Rd/Cpd` | 0.2854 | |
| `a′` | 0.32 | fraction of internal-plume radius between centerline and `z_fc`. Corresponds (via Appendix C geometry) to requiring `a ≈ 0.30` (30 %) of the internal plume cross-sectional area to exceed `z_fc`. Authors note this may be retuned in future. |
| `β` | 0.6 | dynamic-plume entrainment parameter (Briggs) |
| `β′` | 0.4 | internal-plume entrainment parameter (Briggs) |
| `g` | 9.8 m s⁻² | gravity |
| `P0` | 10⁵ Pa (1000 hPa) | reference pressure |
| `φ` (moisture ratio) | 6.67×10⁻⁵ kg kg⁻¹ K⁻¹ | fire moisture-to-heat ratio = **1 g kg⁻¹ of q per 15 K of Δθ** (from Luderer et al. 2009). Note: some early BoM training material said 10:1; 15:1 is correct per the trial report footnote. |
| PFT constant | `π·Cpd·(β′/(1+a′β′))² = 397.3 J kg⁻¹ K⁻¹` | constant bracket of Eq. (25). Check: 3.14159×1005.7×(0.4/1.128)² = 397.4 ✓ |
| `ρ0` (manual method only) | 0.755 kg m⁻³ | fixed density for the quick estimate; 397.3×0.755 ≈ 300 → the "0.3" in Eq. (31) |

Additional thermodynamic constants used in the SP equations (Table 2 of Tory et al. 2018): `Rv = 461.5 J kg⁻¹ K⁻¹`, `Cpv = 1870.0 J kg⁻¹ K⁻¹`, `Cl = 4190 J kg⁻¹ K⁻¹`, `Lv = 2.5×10⁶ J kg⁻¹`.

Algorithm tunables (paper §4b–c):
- Minimum cloud-top temperature: **−20 °C** ("conservative electrification level").
- Buoyancy buffer `δθ_b`: **0.5–1.0 K** (fixed; reference implementation uses 0.5 K). Accounts for buoyancy losses from entrainment/evaporation in the moist plume; forecasters may raise it a few K when the mid-troposphere is very dry.

---

## 3. The core PFT equation (what everything feeds)

From the Briggs heat-flux expression on an internal-plume cross section (Eq. 23) with `z_c = z_fc/(1 + a′β′)` (Eq. 24):

**Eq. (25) — the PFT:**

```
PFT = [ π · Cpd · (β′/(1 + a′β′))² ] · ρ0 · z_fc² · U_ML · Δθ_fc
    =  397.3 · ρ0 · z_fc² · U_ML · Δθ_fc          (SI: W)
```

with

- `z_fc` — free-convection height above (fire) ground, m
- `U_ML` — mean horizontal wind speed (see step 5), m s⁻¹
- `Δθ_fc = θ_pl,fc − θ_ML` — required plume excess potential temperature at `z_fc`, K
- `ρ0` — plume cross-section mean density, kg m⁻³, from:

**Eq. (26):** `ρ0 = Pc/(Rd·θ_pl,fc) · (P0/Pc)^κ` (`Pc` in Pa here; equivalently with hPa: `ρ0 = 100·Pc_hPa/(Rd·θ_pl,fc) · (1000/Pc_hPa)^κ`)

**Eq. (27)/(28):** plume-centerline pressure, from assuming the pressure-depth ratio equals the height ratio `z_fc/z_c = 1 + a′β′ = 1.128`:

```
Pc = P_sfc + (P_fc − P_sfc)/(1 + a′β′)          [Eq. 28]
   = P_sfc − (P_sfc − P_fc)/1.128
```

`θ_pl,fc` is the **plume** potential temperature at the free-convection point (= `θ_SP` at the solution point, step 3), `P_fc` the pressure there.

**Gigawatt form** (as in `metfor::pft`, verified against source):

```
PFT_GW = 397.3 · ρ0 · (z_fc_km)² · U_ML · Δθ_fc / 1000
```

**Eq. (31) — manual/quick approximation** (fixes ρ0 = 0.755 kg m⁻³):

```
PFT(GW) ≈ 0.3 · [z_fc(km)]² · U_ML(m s⁻¹) · Δθ_fc(K)
```

All six published case-study values (§8) use Eq. (31). An automated implementation should compute both: Eq. (25)+(26)+(28) as the primary output, Eq. (31) for comparison against the published anchors.

---

## 4. Full algorithm, step by step

This merges the paper's §4c steps 1–6 with the (independent, third-party) `sounding-analysis` reference implementation, which resolves the numerical details the paper leaves to the reader. Steps marked **[TK21]** are explicit in the paper; **[RI]** are reference-implementation choices.

### Step 1 — Entrained mixed layer (θ_ML, q_ML, ML-LCL) **[TK21 §4c step 1]**

The mixed-layer values must represent the average θ and q **entrained into the plume** during ascent to the ML-LCL. Because Briggs entrained mass flux grows linearly with plume centerline height (Eqs. 15–16: `d(m_flux)/dz = 2ρ0πβ²z_c U`), the averages are **weighted linearly with height**:

```
θ_ML(H) = ∫₀ᴴ θ(h)·h dh / ∫₀ᴴ h dh = (2/H²)·∫₀ᴴ θ(h)·h dh
q_ML(H) = (2/H²)·∫₀ᴴ q(h)·h dh
```

(h = height AGL; trapezoid rule over profile levels is fine: accumulate `(θᵢhᵢ + θᵢ₊₁hᵢ₊₁)·Δh/2` and divide by `H²/2`. **[RI]**)

**ML depth selection (iterative):** grow the candidate ML top `H` level by level, recomputing `θ_ML`, `q_ML` and the corresponding **ML-LCL** each time, until the ML-LCL first lies *within* (at or below) the candidate ML top. Concretely **[RI]**:

1. For each candidate top level `k` (pressure `P_k`): compute `θ_ML`, `q_ML` over `[0, h_k]`; build a surface parcel `T = θ_ML·(P_sfc/P0)^(−κ)` … i.e. temperature of θ_ML at `P_sfc`, dewpoint from `q_ML` at `P_sfc`; compute its LCL pressure `P_LCL(k)` (any standard LCL routine; reference impl iterates T/Td to the saturation point).
2. Find the first pair of consecutive candidates where `P_k ≥ P_LCL(k)` and `P_{k+1} < P_LCL(k+1)` (ML top crosses the LCL). Take `θ_ML`, `q_ML`, `P_top,ML` from the last candidate whose top is still below its own LCL. (Linear interpolation between the two candidates is an acceptable refinement.)
3. **[RI]** requires a minimum ML depth of 50 hPa before the search may terminate (guards against degenerate shallow solutions with strong surface inversions). Not in the paper; recommended.

### Step 2 — Saturation-point (SP) curve **[TK21 §4c step 2; Tory et al. 2018 §3]**

The SP curve is the locus of possible plume condensation points, parameterized by the buoyancy-like parameter `b_T18 = Δθ/θ_ML` (written `β` below, range 0 → ~0.1–0.2):

```
θ_SP(β) = (1 + β)·θ_ML                     [Eq. 29]
q_SP(β) = q_ML + β·φ·θ_ML                  [Eq. 30]   (φ = 6.67e-5 kg/kg/K; q in kg/kg)
```

Each (θ_SP, q_SP) pair defines one saturation point (P_SP, T_SP): the pressure/temperature at which a parcel with that θ and q is exactly saturated. Two equivalent ways to find it:

**(a) Root solve [RI]:** find `P` in [1080, 100] hPa such that `T(θ_SP, P) − Td(q_SP, P) = 0`, where `T(θ_SP,P) = θ_SP·(P/1000)^κ` and `Td(q_SP,P)` inverts the q formula in §2.2. Monotonic in P; bisection converges safely (tolerance: |T−Td| < 0.01 K or ΔP < 0.1 hPa).

**(b) Analytic (Tory et al. 2018 Eqs. 14–17):** evaluate the parcel at `Ps = P0 = 1000 hPa` where `T_pl = θ_SP`; then

```
e_pl = Ps / [(1−ε) + ε/q_SP]                               [T18 Eq. 15]  (vapor pressure, hPa)
T_SP = 2840 / (3.5·ln T_pl − ln e_pl − 4.805) + 55         [T18 Eq. 14]  (Bolton 1980 Eq. 21; T in K)
P_SP = P0 · (T_SP/θ_SP)^K                                  [T18 Eq. 16]
K    = (Cpd/Rd)·[1 + r_pl(Cpv/Cpd)]/[1 + r_pl/ε] ≈ Cpd/Rd  [T18 Eq. 17]  (r_pl = q_pl/(1−q_pl))
```

Both give the same curve to plotting accuracy; (a) is what the reference implementation uses.

### Step 3 — Free-convection condition and `z_fc` **[TK21 §4b assumption 5, §4c step 3]**

For a candidate SP point, the moist plume path above it is the **saturated pseudoadiabat (constant θe) through (P_SP, T_SP)**:

```
θe_SP = θe(T = T_SP, Td = T_SP, P = P_SP)     (saturated parcel)
```

Tory et al. (2018, Eq. 24) define θe via Emanuel (1994, Eq. 4.5.14) at 100 % RH with negligible liquid water:

```
θe = θ · exp[ Lv·r / ((Cpd + Cl·r)·T) ]
```

(Any accurate pseudoadiabat pair — θe definition + its saturated-temperature inversion — is acceptable **provided the same pair is used for both directions**; Bolton (1980) θe with a Wobus/Davies-Jones inversion is fine. This is what metfor does.)

**Free-convection test for a candidate β:** lift along the θe_SP moist adiabat from `P_SP` upward. At each profile level compute the saturated-parcel temperature `T_parcel(P)` (invert θe at P), stop when the **parcel temperature falls below −20 °C** (the minimum-cloud-top/electrification level). The candidate freely convects iff

```
min over the path of [ Tv_parcel(P) − Tv_env(P) ]  ≥  δθ_b      (δθ_b = 0.5 K)
```

i.e. the plume path must be warmer than the environment (with a `δθ_b` margin) **everywhere between the SP point and the −20 °C cloud-top level** — this is how "plume condensation level vs LFC" works in the PFT method: there is no separate LFC lookup; the SP point of the *marginal* (minimum-θe) freely-convecting path **is** the effective LFC, and its height is `z_fc`.

Notes:
- **[RI]** uses virtual temperature for both parcel and environment. The paper compares plain temperatures and neglects vapor buoyancy (stated error < 0.5 % for q < 8 g kg⁻¹). Either is acceptable; Tv is slightly more physical. Be consistent.
- The paper words the buffer as an increment `δθ_b` added to θe_min; the reference implementation equivalently demands the minimum parcel−environment (virtual) temperature difference along the path be ≥ 0.5 °C. Near the marginal path these coincide to first order. Recommended: the min-buoyancy-≥-0.5 K form (simpler, monotone in β).
- Cloud-top criterion ambiguity: paper says "between the SP curve and the T = −20 °C minimum cloud-top height"; **[RI]** terminates where the *parcel* temperature reaches −20 °C. Since the marginal path hugs the environment curve, parcel-based and environment-based −20 °C levels nearly coincide. Recommended: parcel-based (cloud-top temperature is a property of the cloud).

**Search for the marginal β (this defines everything):**

```
β* = min { β ≥ 0 : free-convection test passes }
```

The test is monotone in β (warmer/moister SP ⇒ warmer path). Reference implementation: scan β from 0 in steps of 0.001 up to 0.20 to bracket the transition (last failing β = lo, first passing β = hi), then root-find (bisection/Brent) on `f(β) = min_path[Tv_parcel − Tv_env] − δθ_b` between lo and hi. Convergence: Δβ < 10⁻⁴ (≈ 0.03 K in Δθ_fc) or |f| < 0.01 K. If β = 0 already passes, PFT is limited by the ordinary convective threshold (Δθ_fc → 0 ⇒ PFT → 0: atmosphere supports conventional deep convection from the ML). If no β ≤ 0.2 passes, report "no PFT" (atmosphere cannot support pyroCb for any plausible plume buoyancy; alternatively extend the scan and flag).

At β*: `P_fc = P_SP(β*)`, `T_fc = T_SP(β*)`, `θ_pl,fc = θ_SP(β*)`, `θe_min = θe_SP(β*)`.

**Free-convection height:** `z_fc` = height AGL of `P_fc`, by log-P (or linear **[RI]**) interpolation of the height profile: `z_fc = z(P_fc) − z_sfc`.

### Step 4 — Δθ_fc **[TK21 §4c step 4]**

```
Δθ_fc = θ_pl,fc − θ_ML = β*·θ_ML
```

### Step 5 — U_ML **[TK21 §4c step 5]**

> "In the layer between the surface and z_fc, average the meridional and zonal wind components separately and set U_ML to the magnitude of this averaged wind vector."

```
U_ML = | ( mean(u), mean(v) ) |   over 0 ≤ h ≤ z_fc
```

- **Vector mean, then magnitude** (not mean speed). This deliberately discounts wind that turns with height (see caveats, §9).
- Layer: the paper's calculation step says surface→z_fc (its assumption list and the trial report loosely say "mixed layer"; surface→z_fc is the explicit instruction and what the reference implementation does — recommended).
- Weighting: unweighted (or pressure-weighted trapezoid over the layer **[RI]** — differences are small; unweighted arithmetic mean over interpolated levels is acceptable). No linear-height weighting is prescribed for wind.

### Step 6 — Pc, ρ0, PFT **[TK21 §4c step 6 + Eqs. 25/26/28]**

```
Pc     = P_sfc − (P_sfc − P_fc)/1.128                       [Eq. 28, hPa ok]
ρ0     = (100·Pc)/(Rd·θ_pl,fc) · (1000/Pc)^κ                [Eq. 26, Pc in hPa → ρ0 in kg m⁻³]
PFT    = 397.3 · ρ0 · z_fc² · U_ML · Δθ_fc                  [W;  z_fc in m]
PFT_GW = 397.3 · ρ0 · (z_fc/1000)² · U_ML · Δθ_fc / 1000    [GW]
```

### Reference pseudocode (mirrors `sounding-analysis::fire::pft_analysis`, moisture_ratio = 15.0)

```text
(θ_ML, q_ML, P_sfc, P_topML) = entrained_mixed_layer(profile)        # step 1
(z_fc, P_fc, θ_fc, Δθ_fc, θe_min) = free_convection(profile, 15.0,   # steps 2–4
                                                    θ_ML, q_ML)
U_ML = |vector_mean_wind(profile, 0 .. z_fc)|                        # step 5
Pc   = P_sfc - (P_sfc - P_fc)/(1 + 0.32*0.4)                         # step 6
ρ0   = 100*Pc/(Rd*θ_fc) * (1000/Pc)^(Rd/Cpd)
PFT  = 397.3 * ρ0 * z_fc_km² * U_ML * Δθ_fc / 1000    # GW
```

Failure modes to handle: missing levels; ML-LCL never crossed (saturated profile); no β bracket found; z_fc above data top; profile top warmer than −20 °C.

---

## 5. Output, units, interpretation

- **Units: gigawatts (GW) of total sensible-heat flux entering the plume base** (area-integrated, not W m⁻²). Radiant heat emitted in the combustion zone is excluded (assumed ~30 % of combustion energy; see App. D below).
- **Lower PFT = less firepower needed = more favorable for pyroCb.** PFT is a *threshold*, not a probability: it must be compared (at least mentally) against how much firepower the day's fires could plausibly produce.

**Published interpretation anchors (no formal bin table exists — state this honestly in any UI):**

- **Sir Ivan Fire baseline (Feb 2017, NSW):** PFT ≈ **300 GW** from a forecast sounding at the time an intense pyroCb formed on a wind change under extreme fire danger. The authors: this "might be close to an upper limit for most wildfires, excluding exceptionally large fires" (Chisholm 2001, Canberra 2003, Black Saturday 2009, Gospers Mountain 2019). I.e. **PFT ≳ 300 GW ⇒ pyroCb unlikely except for mega-fires; PFT ≲ 100 GW ⇒ attainable by large (not exceptional) fires; PFT ≲ tens of GW ⇒ attainable by modest fires.**
- **BoM real-time trial (Black Summer 2019/20, ACCESS-R, 6-hourly automated maps):** PFT1 displayed on a **log₂ color scale from 16 to 1024 GW**; values > ~1000 GW treated as very unfavorable; confirmed fire-generated-thunderstorm events spanned nearly **two orders of magnitude** of PFT (~10–1000 GW), which is why the operational product pairs PFT with a fire-danger normalization:
- **PFT1 flag** = ratio of PFT1 to a modified Vesta fire-danger index (Cheney et al. 2012) — flags where low PFT coincides with conditions supporting big fires. A **fuel-moisture > 10 % mask** (fuel moisture from the Vesta near-surface T/RH function) suppresses false flags in cold outbreaks. (Companion product idea for a HRRR implementation: PFT against HDW or hourly wildfire potential.)
- **Firepower yardstick (paper Appendix D, Eq. D1):** observed/plausible firepower from spread rate: `FP = a·h·w_a·(dA/dt)` with `a = 0.7` (fraction of heat entering plume; 30 % radiative loss), `h = 15 MJ kg⁻¹` heat yield, `w_a ≈ 1.25–4 kg m⁻²` fine fuel load, `dA/dt` = burned-area rate (m² s⁻¹). Example scale: a 5-km head fire at 100 MW m⁻¹ ≈ 500 GW.
- Diurnal pattern insight: PFT is typically *extreme* (worst) during peak fire danger (deep, hot, dry ML → huge z_fc) and drops sharply (up to ~10×) around wind changes — pyroCb often form in that window.

---

## 6. Validation anchors (published values to test against)

All were computed with the **manual method, Eq. (31)** (ρ0 = 0.755). Each reproduces exactly from the quoted (z_fc, Δθ_fc, U_ML) — use these as unit tests of the final formula, and the soundings (U. Wyoming archive, station IDs given) as end-to-end tests of the full algorithm (expect agreement in the tens of %, since z_fc/Δθ_fc were read manually off diagrams).

| # | Case | Sounding | z_fc (km) | Δθ_fc (K) | U_ML (m s⁻¹) | Published PFT | Eq. 31 check |
|---|---|---|---|---|---|---|---|
| 1 | Black Saturday (Kilmore East/Murrindindi, VIC) | Melbourne Airport (WMO 94866) 1000 LST 7 Feb 2009 (= 2300 UTC 6 Feb) | 4.8 (after −0.5 km fire-elevation adj.) | 9 | 20 | **1240 GW** | 0.3·4.8²·20·9 = 1244 ✓ |
| 2 | Black Saturday, post-change | Melbourne Airport 2200 LST 7 Feb 2009 (= 1100 UTC) | 3.5 | 8 | 17 | **500 GW** | 499.8 ✓ |
| 3 | Black Saturday, pre-change estimate (extrapolated trace) | derived from #2 | 4.0 | 3 | 20 | **≈290 GW** | 288 ✓ |
| 4 | Chisholm firestorm, AB, Canada | Edmonton Stony Plain (WMO 71119) 0600 LST 28 May 2001 | 3.3 (text: ~3.2; −100 m elev. adj.) | 8 (text: ~7) | 20 | **520 GW** | 522.7 ✓ |
| 5 | Chisholm, afternoon (pyroCb time) | Edmonton Stony Plain 1800 LST 28 May 2001 (= 0000 UTC 29th) | 2.7 | 2.5 | 18 | **100 GW** | 98.4 ✓ (estimated actual firepower ≈ 3250 GW ⇒ 30× exceedance ⇒ violent pyroCb, 3-km overshoot — consistent) |
| 6 | Bald Fire, N. California | mobile radiosonde 2100 PDT 2 Aug 2014 (Lareau & Clements 2016) | 4.4 | 8 | 3 | **140 GW** | 139.4 ✓ |
| 7 | Sedgerly Rd Fire, Inglewood QLD | mobile radiosonde 1741 AEST 5 Dec 2016 | 2.8 | 1 | 5 | **12 GW** | 11.76 ✓ |
| 8 | Sir Ivan Fire, NSW (baseline) | forecast model sounding, Feb 2017 | — | — | — | **≈300 GW** | (no components published) |

Trial-report additional anchors (ACCESS-R automated PFT1): Green Valley–Talmalmo fire, 30 Dec 2019 — PFT1 ≈ 250 GW evening, real-time firepower estimates rising 100→1000 GW as PFT1 fell 500→250 GW (crossover ≈ pyroCb onset ~2 AM); 4 Jan 2020 SE NSW — PFT1 > 1000 GW most of the day (deep MPC only after evening decline); events on both days confirmed by satellite/lightning.

---

## 7. Known limitations and caveats (as stated by the authors)

1. **Constant-wind, neutral-stratification Briggs plume** below cloud base. Errors from shear/stability are only *partially* cancelled by using layer-average U and θ. Strong directional shear is particularly problematic: the vector-mean wind can be near zero while each layer still bends the plume.
2. **No entrainment above condensation** (PFT1 is pure parcel theory in the moist plume). The fixed `δθ_b = 0.5–1 K` buffer is a crude stand-in; dry mid-tropospheres (per Peterson et al. 2017 climatology) deserve a larger buffer. PFT2 (BNHCRC report §7) replaces the parcel path with an entraining path: single entrainment-fraction parameter (plume-mass fraction per km of ascent), layer-by-layer well-mixed + evaporation adjustment, closed by assuming the plume stays saturated; path reconstructed *downward* from the −20 °C cloud-top point to its SP-curve intersection. PFT2 generally raises Δθ_fc and z_fc (PFT2 ≥ PFT1). No peer-reviewed constant set was published for PFT2 — do not implement without the follow-up papers (Tory & Kepert 2023, JAMC "PyroCb Firepower Threshold, Part I/II" — paywalled; not verified here).
3. **Fixed fire moisture ratio** (1 g kg⁻¹ per 15 K). Real fires vary with fuel moisture and radiative losses; sensitivity is modest because the SP curve is dominated by the heat increment.
4. **−20 °C cloud-top threshold over-captures**: intentionally flags electrified towering pyroCu as well as full pyroCb ("deliberately conservative"); it will over-predict lightning-producing storms.
5. **Point-source, circular-neck assumption**: real fires have finite/linear heat sources; assumed valid above the observed "necking" height; may fail for very linear fire fronts (Badlan et al. 2019). PyroCb are associated with "deep flaming" areas rather than thin lines, which supports the assumption.
6. **PFT says nothing about the fire**: identical PFT can be dangerous (drought, heavy fuels, active fires) or irrelevant (green/wet landscape, cold outbreak). Confirmed-event PFTs span ~2 orders of magnitude; pair with a fire-danger/fuel indicator (PFT1-flag, fuel-moisture mask).
7. **Verification is largely qualitative**; computed GW values carry unquantified biases (assumption biases assumed similar across events, so *relative* values and temporal trends are the reliable signal).
8. **Representativity**: a single column at one time; soundings/model columns distant from the fire, terrain elevation differences (adjust z_fc), and timing (pyroCb favorability often peaks briefly near wind changes — 6-hourly output can miss it; prefer hourly HRRR).
9. Radiant-heat losses from the plume above the combustion zone are neglected (standard, after Luderer et al. 2009).
10. Water-vapor buoyancy neglected in the Briggs layer (< 0.5 % error for q < 8 g kg⁻¹).

---

## 8. Ambiguities across sources and recommendations

| Item | Paper | Reference impl. | Recommendation |
|---|---|---|---|
| Wind layer for U_ML | "mixed layer" (assumption 3) vs "surface to z_fc" (step 5) | surface → z_fc | surface → z_fc (explicit calculation step) |
| Wind averaging | vector mean, magnitude | vector mean (pressure-weighted), magnitude | vector mean, magnitude; weighting immaterial — document choice |
| −20 °C level | environment isotherm (figure); text ambiguous | parcel T reaches −20 °C | parcel-based; difference is small |
| Buffer δθ_b | 0.5–1.0 K added to θe_min | min(Tv_p − Tv_e) ≥ 0.5 K along path | min-buoyancy ≥ 0.5 K; expose as parameter |
| Virtual temp. | plain T (vapor buoyancy neglected) | Tv both sides | Tv both sides (or plain T both sides); never mixed |
| β scan range | b_T18 ~ 0–0.1 | 0–0.20, step 0.001 + root refine | 0–0.20 with bisection refinement |
| ML minimum depth | none stated | 50 hPa | 50 hPa, document as guard |
| ML-LCL crossing | "increase until ML-LCL lies within ML" | last step before crossing | either side of crossing or interpolate; differences O(levels) |
| ρ0 | Eq. 26 (exact) or 0.755 (manual) | Eq. 26 | Eq. 26 for product; 0.3-form only for anchor tests |
| θe / moist adiabat | Emanuel 4.5.14 (T18 Eq. 24) | metfor's θe (Bolton-style) + numeric inversion | any self-consistent accurate pseudoadiabat pair |
| Rd | 287.04 | 287.058 | either |

---

## 9. Companion approach: fire-modified parcels (fireCAPE / blow-up analysis)

Two well-defined published variants suitable as a companion product:

### 9.1 fireCAPE / pyroCAPE (Potter 2005; formalized in Tory et al. 2018)

Add fire increments to the **mixed-layer** parcel *at its condensation level*:

```
Δθ_FC = b_SP·θ_env                          [T18 Eq. 19]
Δq_FC = φ·Δθ_FC − 0.14(1−a)·q_env  ≈ φ·Δθ_FC  (small b, a→1)   [T18 Eqs. 20–21]
```

then lift the (θ_ML + Δθ_FC, q_ML + Δq_FC) parcel: fireCAPE = CAPE of that parcel. Potter's original 2 K / 2 g kg⁻¹ (1:1) is now considered unrealistically moist; Luderer et al. (2009) support ~1 g kg⁻¹ per 8–15 K (Tory uses 15). pyroCAPE (the fire's *added* CAPE) has the closed-form estimate:

```
PC = fireCAPE − CAPE = Cpd·(T_SP − T_EL)·Δln θe            [T18 Eq. 22]
   ≈ b_SP·(T_SP − T_EL)·(Lv·φ·θ_env/T_SP + Cpd)            [T18 Eq. 25]
θe = θ·exp[Lv·r/((Cpd + Cl·r)·T)]                          [T18 Eq. 24]
```

Main documented weakness (which motivated the PFT): increment choice is subjective and ignores wind.

### 9.2 Blow-up ΔT analysis (Leach & Gibson 2021, J. Operational Meteor. 9(4), 47–61)

The US-oriented operational variant (implemented in the same `sounding-analysis` crate, experimental module):

- Parcel: **surface-based 100-hPa mixed-layer parcel**, heated by ΔT (scan, e.g. 0–20 °C); two moisture scenarios: (i) dry fire (Δq = 0), (ii) +1 g kg⁻¹ per 8–15 °C of heating (Luderer et al. 2009 Eq. 4, assuming ~50 % radiative loss, 10–60 % fuel moisture).
- Lift dry to LCL then moist-adiabatically; track **integrated buoyancy** `IB(z) = ∫₀ᶻ g·(T_p − T_e)/T_e dz′` (CAPE-type integral per Doswell 2001; paper's Eq. 1 rendered as image — form reconstructed from its caption/definition).
- Outputs: **EL(ΔT)** curve; **Blow-Up ΔT** = heating at which the EL jumps discontinuously (found via max numerical derivative of EL w.r.t. ΔT); **BU Δz** = EL(BUΔT+0.5 °C) − EL(BUΔT−0.5 °C); **MIB** (max integrated buoyancy, ≈ CAPE at the stronger EL); **PMPH** (potential max plume height, where IB returns to 0); % of MIB from latent heat (lift a never-condensing "dry" twin and subtract).
- Interpretation: low BU ΔT ⇒ blow-up/pyroCu transition easily triggered; large BU Δz ⇒ the transition is violent. Use both together.

These parcel diagnostics answer "what happens if the fire adds X" and pair naturally with PFT's "how much must the fire add".

---

## 10. What was NOT found / open items

- **Tory & Kepert JAMC 2023 Parts I & II** (the formal PFT2/automated-PFT papers, if published) were not retrieved; PFT2's entrainment constant is therefore unspecified here. PFT1 as specified above is fully implementable and is what the operational trial ran.
- No US operational PFT product (SPC/CIRA/GSL) was found; NOAA/GSL fire products (HWP, HRRR-smoke) do not compute PFT. The Rust crates (`metfor` ≥0.9, `sounding-analysis` ≥0.17) are the only open-source implementations located.
- No formal published GW bin table exists; §5 anchors are the published interpretive guidance.

---

## 11. Citations

1. Tory, K. J., and J. D. Kepert, 2021: Pyrocumulonimbus Firepower Threshold: Assessing the Atmospheric Potential for pyroCb. *Wea. Forecasting*, 36(2), 439–456. https://doi.org/10.1175/WAF-D-20-0027.1 (open-access PDF: https://journals.ametsoc.org/downloadpdf/journals/wefo/36/2/WAF-D-20-0027.1.pdf)
2. Tory, K. J., W. Thurston, and J. D. Kepert, 2018: Thermodynamics of Pyrocumulus: A Conceptual Study. *Mon. Wea. Rev.*, 146(8), 2579–2598. https://doi.org/10.1175/MWR-D-17-0377.1
3. Tory, K. J., 2020 (pub. 2021): *The Real-Time Trial of the Pyrocumulonimbus Firepower Threshold.* BNHCRC Report No. 694.2021. https://www.naturalhazards.com.au/crc-collection/downloads/the_real-time_trial_of_the_pyrocumulonimbus_firepower_threshold_0.pdf
4. Tory, K. J., 2018: *Models of Buoyant Plume Rise.* BNHCRC Research Report No. 451. https://www.bnhcrc.com.au/publications/biblio/bnh-5267
5. Tory, K. J., 2019: Pyrocumulonimbus firepower threshold: A pyroCb prediction tool. *AJEM Monograph No. 5*, 21–27. https://www.aidr.org.au/media/7379/monograph-no5-extended-abstracts-final.pdf
6. Tory, K. J., and M. Peace, 2022: PFT: Selected learnings from the 'Black Summer' real-time trial. *Advances in Forest Fire Research 2022.* https://www.researchgate.net/publication/364765687
7. Leach, R. N., and C. V. Gibson, 2021: Assessing the Potential for Pyroconvection and Wildfire Blow Ups. *J. Operational Meteor.*, 9(4), 47–61. https://doi.org/10.15191/nwajom.2021.0904 (PDF: http://nwafiles.nwas.org/jom/articles/2021/2021-JOM4/2021-JOM4.pdf)
8. Luderer, G., J. Trentmann, and M. O. Andreae, 2009: A new look at the role of fire-released moisture on the dynamics of atmospheric pyro-convection. *Int. J. Wildland Fire*, 18, 554–562. https://doi.org/10.1071/WF07035
9. Potter, B. E., 2005: The role of released moisture in the atmospheric dynamics associated with wildland fires. *Int. J. Wildland Fire*, 14, 77–84. https://doi.org/10.1071/WF04045
10. Peterson, D. A., et al., 2017: A conceptual model for development of intense pyrocumulonimbus in western North America. *Mon. Wea. Rev.*, 145, 2235–2255. https://doi.org/10.1175/MWR-D-16-0232.1
11. Briggs, G. A., 1975: Plume rise predictions. *Lectures on Air Pollution and Environmental Impact Analyses*, AMS, 59–111. / Briggs, G. A., 1984: Plume rise and buoyancy effects. *Atmospheric Science and Power Production*, DOE/TIC-27601, 327–366.
12. Reference implementation: `rnleach/metfor` (fn `pft`, `src/functions.rs`) https://docs.rs/metfor/latest/metfor/fn.pft.html and `rnleach/sounding-analysis` (`src/fire.rs`, fn `pft_analysis`) https://docs.rs/sounding-analysis/latest/sounding_analysis/fn.pft_analysis.html — MIT license; verified against the paper in this spec.
13. Soundings for validation anchors: University of Wyoming archive, http://weather.uwyo.edu/upperair/sounding.html (Melbourne Airport 94866; Edmonton Stony Plain 71119).
