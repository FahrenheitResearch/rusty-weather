# CAFire Weather — Product Guide: What Each Plot Is & How It's Computed

This guide explains, product by product, what each CAFire Weather plot shows, how it is calculated, and the scientific reference behind it. It is written for a science-literate reader whose question is simply "what is this plot?" — so each entry gives the physical meaning, the exact computational recipe, and a citation. It deliberately does **not** describe where the underlying data comes from or how it is obtained; that is covered in a separate internal document. Throughout, named models (HRRR, RTMA, LANDFIRE) appear only as the computational basis for a field, never as a data source.

---

## Standard surface weather maps

### 2 m Temperature
- **What it is:** Air temperature 2 m above the ground — the standard "shade" temperature you would read off a thermometer at head height. Warmer colors mark hotter air; it anchors most fire-weather reasoning, since heat drives drying and instability.
- **How it's computed:** Read straight from the HRRR model's forecast 2 m air-temperature field. No derivation — the only transform is a unit conversion (kelvin to degrees C/F) for display.
- **Reference:** Native model field; no scientific derivation.

### 2 m Dewpoint
- **What it is:** The temperature to which the 2 m air would have to cool to become saturated. It is a direct, absolute measure of how much moisture is in the near-surface air — higher dewpoint means moister (and, for fire, less receptive) low levels.
- **How it's computed:** Read straight from the HRRR model's forecast 2 m dewpoint field. No derivation beyond a display unit conversion.
- **Reference:** Native model field; no scientific derivation.

### 2 m Relative Humidity
- **What it is:** The moisture in the 2 m air expressed as a percentage of what the air could hold at saturation for its current temperature. Low RH is a core fire-weather ingredient because dry air pulls moisture out of fuels.
- **How it's computed:** Read straight from the HRRR model's forecast 2 m relative-humidity field. (A relative humidity is separately re-derived from temperature and dewpoint purely to feed the Fosberg/HDW fire indices, but the RH map product itself is the native model field.)
- **Reference:** Native model field; no scientific derivation.

### 10 m Wind
- **What it is:** Wind at 10 m above the ground — the standard anemometer height — drawn as barbs that show both direction and speed over the underlying surface fill. Wind steers fire spread and drives the "windy" part of fire-weather danger.
- **How it's computed:** Uses the HRRR model's native 10 m eastward (u) and northward (v) wind components directly. Barbs are drawn from (u, v); wherever a scalar speed is needed it is the vector magnitude √(u² + v²). There is no standalone wind-speed color fill.
- **Reference:** Native model field; no scientific derivation.

### 10 m Wind Gusts
- **What it is:** The peak (gust) wind magnitude near the surface — the brief bursts that exceed the sustained wind and often do the damage in fire runs and blowups.
- **How it's computed:** Read straight from the HRRR model's forecast surface wind-gust diagnostic. Native field, no derivation.
- **Reference:** Native model field; no scientific derivation.

### Visibility
- **What it is:** Horizontal visibility at the surface — how far you can see — which drops in fog, heavy precipitation, blowing dust, or thick smoke.
- **How it's computed:** Read straight from the HRRR model's forecast surface-visibility diagnostic. Native field, no derivation.
- **Reference:** Native model field; no scientific derivation.

### Near-Surface Smoke (PM2.5, 8 m)
- **What it is:** Mass concentration of fine smoke particulate (PM2.5) at roughly breathing level (~8 m above ground) — a proxy for the smoke you would actually be inhaling at the surface. Available on the smoke-coupled HRRR only.
- **How it's computed:** Read straight from the smoke-coupled HRRR model's near-surface (8 m) smoke mass-density field. Native field, no derivation.
- **Reference:** Native model field; no scientific derivation.

### Column-Integrated Smoke
- **What it is:** Total smoke loading integrated through the entire depth of the atmosphere above each point — it reveals plumes aloft even when the surface air is relatively clear, useful for tracking transported smoke.
- **How it's computed:** Read straight from the smoke-coupled HRRR model's vertically-integrated (whole-atmosphere column) smoke mass field. Native field, no derivation.
- **Reference:** Native model field; no scientific derivation.

### 2 m Vapor Pressure Deficit (VPD)
- **What it is:** The gap between how much water vapor the 2 m air could hold at saturation and how much it actually holds, in hectopascals (hPa). It is the atmosphere's drying power (evaporative demand): the larger the deficit, the harder the air pulls moisture from live and dead fuels, so higher VPD is more fire-favorable.
- **How it's computed:** Derived from HRRR near-surface temperature and moisture. VPD = e_s(T_2m) − e(Td_2m), clamped to ≥ 0 hPa, where e_s is the saturation vapor pressure at the 2 m temperature and e(Td_2m) is the vapor pressure at the 2 m dewpoint (numerically, e_s evaluated at the dewpoint). The saturation vapor pressure e_s uses the Ambaum (2020) liquid-water formulation (the same formula MetPy uses). The 2 m dewpoint that feeds e(Td_2m) is recovered from the model's 2 m humidity via the Bolton (1980) inversion, Td = 243.5·ln(e/6.112) / (17.67 − ln(e/6.112)).
- **Reference:** Ambaum (2020) for the saturation vapor pressure as implemented; Bolton (1980) for the dewpoint recovery.

### Hot-Dry-Windy Index (HDW)
- **What it is:** A single fire-weather number that multiplies the atmosphere's drying power by its wind, so it spikes when hot, dry, and windy conditions occur together — the combination that most readily makes fire behavior erratic and hard to control. Note this is a surface proxy (2 m moisture × 10 m wind), not the canonical version that scans the lowest 500 m.
- **How it's computed:** Derived from HRRR surface fields as HDW = VPD_2m × wind_10m, where VPD_2m is the 2 m vapor pressure deficit in hPa (computed as in the VPD product) and wind_10m is the 10 m wind speed √(u² + v²) in m/s; result in hPa·m/s. The canonical Srock et al. HDW instead multiplies the maximum VPD by the maximum wind speed found within the lowest ~500 m of the atmosphere — the two maxima located independently and possibly at different levels — rather than multiplying co-located surface values; this product substitutes the 2 m/10 m surface values as a fast proxy.
- **Reference:** Srock et al. (2018).

### Fire Weather Composite
- **What it is:** A public-facing 0–100 fire-weather danger score that fuses two established weather-only fire indices into one easy-to-read scale (higher = more dangerous). It is a weather-only blend — it says nothing about how dry the fuels or landscape actually are.
- **How it's computed:** Derived from HRRR surface fields as composite = clamp(0.5 · FFWI + 0.5 · min(HDW, 100), 0, 100). FFWI is the Fosberg Fire Weather Index from 2 m temperature (in °F), 2 m relative humidity (%), and 10 m wind (in mph): equilibrium moisture content m from Fosberg's piecewise fit in RH and T, a moisture-damping factor η = 1 − 2m + 1.5m² − 0.5m³ (with m = EMC/30), then FFWI = η · √(1 + U²) · (10/3), clamped to 0–100. HDW is the surface Hot-Dry-Windy value (VPD_2m × wind_10m) capped at 100 before blending so it shares the composite's 0–100 range, while the standalone HDW product keeps its native units.
- **Reference:** Fosberg (1978) for the FFWI component; Srock et al. (2018) for the HDW component.

---

## Day-window extreme maps

These maps fold the model's hourly output over a chosen day-window (0–24 h = F001–F024, 24–48 h = F025–F048, 0–48 h = F001–F048; the 24 h/48 h windows require an extended HRRR cycle that reaches F24/F48) into a single grid. Unless noted, the fold is a pointwise (cell-by-cell) maximum, minimum, or their difference over the hours in the window, with no smoothing or interpolation.

### 2 m Temperature — window maximum
- **What it is:** The warmest 2 m air temperature each grid cell is forecast to reach anywhere within the chosen day-window — a map of the peak (usually afternoon) heat over the next 0–24 h, 24–48 h, or full 0–48 h.
- **How it's computed:** Each hourly 2 m temperature field is decoded and converted from kelvin to degrees C (minus 273.15), then folded cell-by-cell by pointwise maximum across all hours in the window. Each cell keeps the hottest hourly value it saw. Displayed in °C.
- **Reference:** Native model field; a plain temporal maximum, no external algorithm.

### 2 m Temperature — window minimum
- **What it is:** The coldest 2 m air temperature each grid cell is forecast to reach within the window — a map of the overnight/early-morning low.
- **How it's computed:** Identical to the temperature maximum but folded with a pointwise minimum; the smallest hourly value at each grid cell is kept. Displayed in °C.
- **Reference:** Native model field; a plain temporal minimum, no external method.

### 2 m Temperature — diurnal range
- **What it is:** The spread between the warmest and coldest 2 m temperature within the window — how large the day's temperature swing is. Big values flag strong diurnal heating/cooling (clear, dry, light-wind regimes); small values flag well-mixed or cloudy air masses.
- **How it's computed:** The window's pointwise max and pointwise min (each in °C) are subtracted cell-by-cell: range = max − min. Units are °C (a difference).
- **Reference:** Derived as (window max − window min); a single arithmetic difference, no external citation.

### 2 m Relative Humidity — window maximum
- **What it is:** The highest 2 m relative humidity each cell reaches within the window — typically the overnight moisture-recovery peak, useful for gauging how much humidity recovers over a fire-weather period.
- **How it's computed:** Each hourly 2 m RH field is decoded, clamped to the physical range 0–100 %, and folded by pointwise maximum across the hours. Displayed in percent.
- **Reference:** Native model field; a plain temporal maximum with a 0–100 % clamp.

### 2 m Relative Humidity — window minimum
- **What it is:** The lowest 2 m relative humidity each cell reaches within the window — the driest moment of the period, a core fire-weather quantity.
- **How it's computed:** Same inputs as the RH maximum (clamped 0–100 %) but folded with a pointwise minimum, so each cell keeps its single driest hourly value. Displayed in percent.
- **Reference:** Native model field; a plain temporal minimum.

### 2 m Relative Humidity — range
- **What it is:** The spread between the highest and lowest 2 m RH within the window — how much RH climbs and falls over the period.
- **How it's computed:** The window's pointwise RH max and min (each clamped 0–100 %) are subtracted cell-by-cell: range = max − min. Units are percent (a difference).
- **Reference:** Derived as (window max − window min); a single subtraction, no external citation.

### 2 m Dewpoint — window maximum
- **What it is:** The highest 2 m dewpoint each cell reaches within the window — the moistest low-level air (peak absolute moisture) over the period.
- **How it's computed:** Each hourly 2 m dewpoint field is converted kelvin to °C (minus 273.15) and folded by pointwise maximum. Displayed in °C.
- **Reference:** Native model field; a plain temporal maximum.

### 2 m Dewpoint — window minimum
- **What it is:** The lowest 2 m dewpoint each cell reaches within the window — the driest low-level air mass (minimum absolute moisture) over the period.
- **How it's computed:** Same as the dewpoint maximum (converted to °C) but folded with a pointwise minimum. Displayed in °C.
- **Reference:** Native model field; a plain temporal minimum.

### 2 m Dewpoint — range
- **What it is:** The spread between the highest and lowest 2 m dewpoint within the window — how much low-level moisture varies (large ranges often accompany dry-air advection or mixing).
- **How it's computed:** The window's pointwise dewpoint max and min (in °C) are subtracted cell-by-cell: range = max − min. Units are °C (a difference).
- **Reference:** Derived as (window max − window min); a single subtraction, no external citation.

### 2 m Vapor Pressure Deficit — window maximum
- **What it is:** The peak 2 m VPD each cell reaches within the window — the moment of greatest atmospheric drying power (evaporative demand). High VPD is strongly tied to fire spread and fuel drying.
- **How it's computed:** VPD is computed at each forecast hour, then folded. Per hour, the preferred path uses 2 m temperature and 2 m RH: VPD = e_s(T) × (1 − RH/100), where e_s(T) = 6.112 · exp(17.67·T_C / (T_C + 243.5)) hPa (Bolton 1980). If RH is unavailable it falls back to VPD = e_s(T) − e_s(Td), clamped ≥ 0. The hourly VPD grids are folded by pointwise maximum. Units hPa.
- **Reference:** Bolton (1980) for the saturation-vapor-pressure formula; window fold is a pointwise maximum.

### 2 m Vapor Pressure Deficit — window minimum
- **What it is:** The lowest 2 m VPD each cell reaches within the window — the most humid, least drying moment (e.g. overnight, when evaporative demand relaxes).
- **How it's computed:** Per-hour VPD is computed exactly as for the VPD maximum (Bolton 1980 e_s, with the e_s(T) − e_s(Td) fallback clamped ≥ 0), then folded by pointwise minimum. Units hPa.
- **Reference:** Bolton (1980); window fold is a pointwise minimum.

### 2 m Vapor Pressure Deficit — range
- **What it is:** The spread between the peak and lowest 2 m VPD within the window — how much the atmosphere's drying power swings over the period.
- **How it's computed:** Per-hour VPD (Bolton 1980, dewpoint fallback clamped ≥ 0) is formed for every hour; the window's pointwise VPD max and min are subtracted cell-by-cell: range = max − min. Units hPa (a difference).
- **Reference:** Bolton (1980) underlies each hourly VPD; the range is the arithmetic difference of the two window extrema.

### 10 m Wind Speed — window maximum
- **What it is:** The strongest 10 m sustained wind speed each cell is forecast to see within the window. In this family wind has max-only variants (no min or range).
- **How it's computed:** The model reports, at each forecast hour, the maximum 10 m wind speed over that hour (carried in HRRR GRIB2 as the u/v components MAXUW/MAXVW of the hourly maximum 10 m wind, with speed √(MAXUW² + MAXVW²)). These hourly maxima are folded by pointwise maximum — a maximum of hourly maxima — so each cell keeps the largest within-hour wind seen anywhere in the window. Converted from m/s to knots (× 1.9438445). Displayed in knots.
- **Reference:** Native hourly-maximum model field; a pointwise maximum of the model's own hourly maxima plus a unit conversion.

### QPF — 1 h
- **What it is:** Quantitative precipitation forecast: liquid-equivalent precipitation accumulated over the trailing 1 hour ending at the forecast hour.
- **How it's computed:** The model's 1-hour surface accumulated-precipitation (APCP) bucket ending at the requested hour is decoded and converted mm → inches (÷ 25.4). A single native 1 h bucket; no folding. Displayed in inches.
- **Reference:** Native APCP field; only a mm-to-inch conversion.

### QPF — 6 h
- **What it is:** Liquid-equivalent precipitation accumulated over the trailing 6 hours ending at the forecast hour.
- **How it's computed:** If a direct native 6-hour APCP bucket exists at the forecast hour, it is used as-is; otherwise six consecutive native 1-hour APCP buckets spanning the trailing 6 h (F−5 through F) are summed. Converted mm → inches (÷ 25.4). Displayed in inches.
- **Reference:** Native APCP field; where no direct bucket exists, a simple sum of consecutive hourly buckets plus a unit conversion.

### QPF — 24 h
- **What it is:** Liquid-equivalent precipitation accumulated over the trailing 24 hours ending at the forecast hour. Requires an extended HRRR cycle reaching F24.
- **How it's computed:** If a direct native 24-hour APCP bucket is available it is used directly; otherwise 24 consecutive native 1-hour APCP buckets over the trailing 24 h (F−23 through F) are summed. Converted mm → inches (÷ 25.4). Displayed in inches.
- **Reference:** Native APCP field; a native bucket or a simple sum of consecutive hourly buckets plus a unit conversion.

---

## Fuel-dryness / fire-danger products

### Energy Release Component (ERC)
- **What it is:** The standard NFDRS fuel-dryness / available-energy index: a unitless number proportional to the potential heat energy (BTU per square foot) released in the flaming front of a fire. Because it is dominated by the heavy, slow-drying fuels, it climbs through a dry season and drops only after soaking rains, making it the go-to seasonal fuel-dryness signal — higher ERC means drier fuels and a hotter, harder-to-control fire.
- **How it's computed:** An NFDRS-1978 output. From a fuel model's loadings and heat content plus the modeled moisture of every dead timelag class (1-, 10-, 100-, 1000-hr) and the live herbaceous and woody classes, NFDRS evaluates Rothermel's reaction intensity (heat release per unit area) and multiplies it by the flaming-zone burnout/residence time to get available energy per unit area, scaled to the open-ended ERC index. Wind and slope are deliberately excluded, so ERC responds only to fuel moisture and loading, weighted heavily toward the 1000-hr fuels that carry the drought signal.
- **Reference:** Bradshaw et al. (1983), USDA GTR INT-169, building on Deeming et al. (1977), GTR INT-39.

### Burning Index (BI)
- **What it is:** The NFDRS index of how hard a fire would be to contain — a unitless number roughly ten times the predicted flame length in feet at the head of a fire. It blends how fast a fire would spread with how much energy it would release.
- **How it's computed:** An NFDRS-1978 output. It combines the Spread Component SC (Rothermel's forward rate of spread in ft/min, a function of fuel model, dead and live fuel moisture, wind, and slope) with the ERC via BI = 3.01 × (SC × ERC)^0.46, rounded to an integer. The 0.46 exponent and 3.01 constant calibrate BI to about ten times the Byram flame length in feet.
- **Reference:** Bradshaw et al. (1983), USDA GTR INT-169, building on Deeming et al. (1977), GTR INT-39.

### Keetch-Byram Drought Index (KBDI)
- **What it is:** A cumulative soil- and duff-moisture-deficit index on a 0–800 scale (0 = saturated soil, 800 ≈ eight inches of moisture deficit). High values mean the deep fuels, duff, and organic soil layers have dried out and are available to burn, and that mop-up will be difficult.
- **How it's computed:** Computed with the Keetch-Byram (1968) bookkeeping, run forward day by day over a 180-day spin-up. Each day: (1) rain wets the soil — for days whose precipitation exceeds a 0.20-inch canopy threshold, net rain = (daily precip − 0.20 in) and the index drops 100 points per inch of net rain, KBDI ← max(0, KBDI − 100·(rain − 0.20)); (2) drying adds the drought factor dQ = [(800 − Q) · (0.9676·exp(0.0486·T) − 8.299)] / [1 + 10.88·exp(−0.0441·R)] × 1e−3, where Q is the current index, T is the daily maximum temperature in °F (drying accrues only above 50 °F), and R is mean annual rainfall in inches. The result is clamped 0–800. R is currently a fixed scalar (default 20 in), not yet gridded. Driven by daily maximum temperature and precipitation.
- **Reference:** Keetch & Byram (1968), USDA Research Paper SE-38 (revised 1988).

### Dead Fuel Moisture (1-, 10-, 100-, 1000-hour)
- **What it is:** The modeled water content (percent of oven-dry weight) of dead vegetation, split by how quickly each size class responds to weather: 1-hr = fine flashy fuels (grass, litter, under 1/4 in) that equilibrate within an hour; 10-hr = 1/4–1 in twigs; 100-hr = 1–3 in branchwood; 1000-hr = 3–8 in logs that integrate weeks of weather. Lower moisture means more ignitable, more available fuel.
- **How it's computed:** The NFDRS timelag fuel-moisture model. The 1-hr and 10-hr contents are driven toward the equilibrium moisture content (EMC), a function of air temperature and relative humidity (adjusted for solar heating / state of the weather), relaxing on their nominal ~1-hr and ~10-hr response times. The 100-hr and 1000-hr contents are integrated from a boundary condition built from 24-hour (100-hr) and 7-day running (1000-hr) averages of temperature, humidity-derived EMC, and daily precipitation duration, relaxing on the longer timelags.
- **Reference:** Bradshaw et al. (1983), USDA GTR INT-169, building on Deeming et al. (1977), GTR INT-39.

### LANDFIRE Fuel Model
- **What it is:** A static, per-pixel classification of the surface fuel type on the ground (for example short grass, timber litter, brush, or slash) — the fuel-bed template that spread and intensity models key off. It changes only when the fuel landscape is remapped, not with the weather.
- **How it's computed:** Read directly from the LANDFIRE surface fuel-model layer, a nationally consistent 30-m raster that classifies each pixel into a standard fire-behavior fuel model (Anderson's 13 or Scott and Burgan's 40) from mapped existing vegetation type, canopy cover, height, and disturbance history. No calculation beyond regridding onto the map grid.
- **Reference:** Rollins (2009).

### LANDFIRE Fuel Loading
- **What it is:** A static map of how much burnable surface fuel is present, in tons per acre — the mass of the fuel bed, complementary to the fuel-model class.
- **How it's computed:** Read directly from the LANDFIRE fuel-loading layer, a nationally consistent 30-m raster of surface fuel loading derived from LANDFIRE's mapped vegetation and assigned fuel-model layers. No calculation beyond regridding onto the map grid.
- **Reference:** Rollins (2009).

### Fuel Receptiveness
- **What it is:** A single 0–100 fuel-dryness score that folds all available fuel indices into one "how ready are the fuels to burn" number; higher means drier and more receptive.
- **How it's computed:** The unweighted mean, at each cell, of whichever fuel grids are present, each first mapped to a 0–100 "drier = higher" score: ERC clamped to 0–100; KBDI linearly mapped 0–800 → 0–100; Burning Index linearly mapped 0–150 → 0–100; and each dead-fuel-moisture class inverted so dry scores high via (wet − value)/(wet − critical)×100 with class-specific endpoints (critical/wet, in %: 1-hr 2/25, 10-hr 3/30, 100-hr 5/35, 1000-hr 6/40). The finite component scores are averaged and clamped to 0–100; at least one fuel grid must be present.
- **Reference:** Composite of NFDRS/KBDI fuel outputs — Bradshaw et al. (1983) and Keetch & Byram (1968).

### Fire Potential Composite
- **What it is:** A 0–100 blend of fire weather and fuel dryness — the "both the atmosphere and the fuels are primed" score.
- **How it's computed:** A weighted average, 0.58 × Fire Weather Composite + 0.42 × Fuel Receptiveness, clamped to 0–100 (both inputs must be finite). The Fire Weather Composite is the model-derived 0–100 weather blend clamp(0.5 × FFWI + 0.5 × HDW-capped-at-100, 0–100), where FFWI is computed from 2 m temperature, RH, and 10 m wind, and HDW = 2 m VPD × 10 m wind speed. Fuel Receptiveness is the fuel-dryness score above.
- **Reference:** Weather side — Srock et al. (2018), Fosberg (1978); fuel side — Bradshaw et al. (1983) (NFDRS).

### HDW × Fuel Receptiveness
- **What it is:** Flags where a hot, dry, windy atmosphere coincides with dry, receptive fuels. The geometric mean stays low unless both weather and fuels are elevated, so it isolates the joint danger.
- **How it's computed:** Geometric mean of two 0–100 scores: √((HDW_score/100) × (receptiveness/100)) × 100. HDW (= 2 m VPD × 10 m wind speed, hPa·m/s) is normalized to 0–100 linearly over 0–800; Fuel Receptiveness is the score above. NaN where either input is missing.
- **Reference:** HDW — Srock et al. (2018); fuel side — Bradshaw et al. (1983) (NFDRS).

### VPD × Fuel Receptiveness
- **What it is:** Flags where high atmospheric moisture demand (VPD) overlaps dry fuels; the geometric mean highlights the cells where both are high at once.
- **How it's computed:** Geometric mean √((VPD_score/100) × (receptiveness/100)) × 100. The 2 m VPD (saturation vapor pressure at air temperature − actual vapor pressure at dewpoint, floored at 0, in hPa) is normalized 0–100 linearly over 0–60 hPa; Fuel Receptiveness is the score above. NaN where either input is missing.
- **Reference:** VPD as a fire-related quantity — Seager et al. (2015); fuel side — Bradshaw et al. (1983) (NFDRS).

### ERC × HDW Composite
- **What it is:** Combines the seasonal fuel-energy signal (ERC) with the instantaneous fire-weather signal (HDW) into one geometric-mean index — it goes high only when long-term fuel dryness and short-term hot-dry-windy conditions line up.
- **How it's computed:** Geometric mean √((ERC_score/100) × (HDW_score/100)) × 100, where ERC is clamped to 0–100 and HDW (= 2 m VPD × 10 m wind speed) is normalized 0–100 linearly over 0–800. NaN where either input is missing.
- **Reference:** ERC — Bradshaw et al. (1983), Deeming et al. (1977) (NFDRS); HDW — Srock et al. (2018).

---

## RTMA anomaly suite & climatology browser

This family answers "how unusual is today's forecast here?" against a stored empirical climatology. It comprises a shared ranking engine, individual seasonal anomaly maps, an all-time-record companion for each, and a browser for the climatology reference maps themselves.

### How the ranking works (shared method for the whole suite)
- **What it is:** The common engine behind every anomaly map: it places the day's forecast value in the local historical distribution and reports a percentile — roughly, the fraction of past years this value would beat at this cell and season.
- **How it's computed:** At each grid cell the climatology stores eight empirical percentile anchors — p05, p10, p25, p50, p75, p90, p95, p99 — of the variable's distribution, computed as empirical quantiles of the 2019–2026 RTMA 2.5 km surface-analysis record. For the seasonal baseline these come from a ±7-day day-of-year window (~105 samples per cell); for the vs-record baseline, from every analyzed day (~2695). The HRRR forecast value is placed on this empirical CDF by piecewise-linear interpolation between the eight anchors, yielding a rank in [5, 99]. Values below p05 clamp to 5 and above p99 clamp to 99 (the tails carry no stored shape); flat CDF spans (equal anchors, e.g. mostly-zero joint-hour cells) rank at the span midpoint; quantization wiggles are repaired with a running maximum so the ladder stays monotone. "Dryness"/"recovery" products invert the scale (rank → 100 − rank + 4) so the dangerous low-RH tail reads 99.
- **Reference:** Empirical-CDF percentile ranking / linear-interpolated sample quantiles — Hyndman & Fan (1996); cf. Wilks (2019). Climatological basis — RTMA (De Pondeca et al. 2011).

### Day-Max VPD Percentile
- **What it is:** Where the day's peak atmospheric moisture demand (VPD) falls relative to what is normal at this cell for this time of year. High percentiles mean an unusually thirsty, fire-favorable atmosphere.
- **How it's computed:** For each hour of the UTC calendar day (00–23Z, requiring ≥ 20 of 24 hours), HRRR 2 m temperature and dewpoint give hourly VPD = max(e_s(T) − e_s(Td), 0) × 0.1 kPa, with e_s(T_C) = 6.112 · exp(17.67·T_C / (T_C + 243.5)) hPa. The day-maximum VPD is ranked against the stored day-of-year anchors via the shared empirical-CDF method.
- **Reference:** VPD — Bolton (1980); ranking — Hyndman & Fan (1996); climatology — RTMA (De Pondeca et al. 2011).

### Day-Min RH Dryness Percentile
- **What it is:** How extreme the day's lowest relative humidity is compared with normal, oriented so the driest days read near 99. It flags days whose afternoon dryness is unusual for the season.
- **How it's computed:** Hourly 2 m RH = clamp(100 × e_s(Td)/e_s(T), 0, 100) over the 00–23Z window (≥ 20 of 24 hours), with e_s the Bolton/Magnus formula. The day-minimum RH is ranked against the stored anchors, with the percentile inverted (rank → 100 − rank + 4) so an at-or-below-p05 minimum RH maps to 99 and an at-or-above-p99 minimum maps to 5.
- **Reference:** RH — Bolton (1980); ranking — Hyndman & Fan (1996); climatology — RTMA (De Pondeca et al. 2011).

### Day-Max Wind Percentile
- **What it is:** Where the day's strongest sustained 10 m wind ranks against the local seasonal climatology. High percentiles indicate an unusually windy day for the date.
- **How it's computed:** Hourly sustained wind speed = √(u² + v²) from the HRRR 10 m u/v components over 00–23Z (≥ 20 of 24 hours); the day-maximum is ranked against the stored anchors via the shared empirical-CDF method.
- **Reference:** Wind speed is the vector magnitude of the model 10 m u/v; ranking — Hyndman & Fan (1996), cf. Wilks (2019); climatology — RTMA (De Pondeca et al. 2011).

### Day-Max Gust Percentile
- **What it is:** Where the day's peak 10 m wind gust ranks against normal for the cell and season. High percentiles flag unusually gusty conditions.
- **How it's computed:** The day-maximum of the HRRR hourly 10 m wind gust over 00–23Z (≥ 20 of 24 hours) is ranked against the stored anchors via the shared empirical-CDF method.
- **Reference:** Gust is the model's 10 m gust field; ranking — Hyndman & Fan (1996), cf. Wilks (2019); climatology — RTMA (De Pondeca et al. 2011).

### Day-Max Surface HDW (Wind) Percentile
- **What it is:** Where the day's peak surface Hot-Dry-Windy proxy — moisture demand times sustained wind — ranks against normal. It highlights days whose combined heat/dryness/wind loading is unusual for the season.
- **How it's computed:** Each hour forms a surface HDW proxy = VPD (kPa) × sustained wind (m/s), with VPD from the Bolton saturation vapor pressure and wind = √(u² + v²). The day-maximum over 00–23Z (≥ 20 of 24 hours) is ranked against the stored anchors. This is a 2 m/10 m surface proxy, not the canonical HDW (which maxes VPD × wind over the lowest 500 m).
- **Reference:** HDW concept — Srock et al. (2018); VPD — Bolton (1980); ranking — Hyndman & Fan (1996); climatology — RTMA (De Pondeca et al. 2011).

### Day-Max Surface HDW (Gust) Percentile
- **What it is:** Same as the surface HDW (wind) percentile but using peak gust instead of sustained wind, so it weights the day's gustiest fire-weather loading against climatology.
- **How it's computed:** Each hour forms a surface HDW proxy = VPD (kPa) × 10 m gust (m/s); the day-maximum over 00–23Z (≥ 20 of 24 hours) is ranked against the stored anchors. A surface proxy of HDW, not the lowest-500 m formulation.
- **Reference:** HDW — Srock et al. (2018); VPD — Bolton (1980); ranking — Hyndman & Fan (1996); climatology — RTMA (De Pondeca et al. 2011).

### Hours RH ≤ 15 % & Gust ≥ 25 mph Percentile
- **What it is:** How unusual the day's count of critically dry-and-gusty hours is versus climatology — hours simultaneously at or below 15 % RH and at or above 25 mph gust. High percentiles mark an exceptional red-flag-type day.
- **How it's computed:** Over 00–23Z, count the hours where RH ≤ 15 % AND gust ≥ 25 mph (25 mph = 11.176 m/s; RH from the Bolton formula, gust from the model). The count is ranked against the stored anchors. Where the climatology itself is essentially zero (p75 anchor ≤ 0) and the forecast count is zero, the cell renders as no-signal rather than a mid-rank fill.
- **Reference:** RH — Bolton (1980); ranking with flat-span midpoint handling — Hyndman & Fan (1996); climatology — RTMA (De Pondeca et al. 2011).

### Hours RH ≤ 20 % & Gust ≥ 25 mph Percentile
- **What it is:** The same joint-threshold-hours anomaly as above but at the slightly less severe 20 % RH threshold, giving a broader count of critically dry-and-gusty hours ranked against normal.
- **How it's computed:** Count hours over 00–23Z with RH ≤ 20 % AND gust ≥ 25 mph (11.176 m/s), then rank against the stored anchors. Same zero-where-climatologically-zero no-signal rule as the RH ≤ 15 % product.
- **Reference:** RH — Bolton (1980); ranking — Hyndman & Fan (1996); climatology — RTMA (De Pondeca et al. 2011).

### Hours RH ≤ 20 % & Wind ≥ 20 mph Percentile
- **What it is:** How unusual the day's count of dry-and-windy hours is (sustained-wind version) — hours at or below 20 % RH and at or above 20 mph sustained wind — ranked against climatology.
- **How it's computed:** Count hours over 00–23Z with RH ≤ 20 % AND sustained wind = √(u² + v²) ≥ 20 mph (20 mph = 8.9408 m/s), then rank against the stored anchors. Same no-signal-where-climatologically-zero rule.
- **Reference:** RH — Bolton (1980); ranking — Hyndman & Fan (1996); climatology — RTMA (De Pondeca et al. 2011).

### Overnight RH Recovery Percentile (12Z–06Z)
- **What it is:** How unusual the overnight humidity recovery is: it ranks the minimum RH over the 12Z-to-06Z-next-morning window, oriented so poor recovery (persistently low overnight RH) reads extreme. Poor recovery keeps fuels receptive through the night.
- **How it's computed:** Over the overnight window (12Z of the target date through 06Z the next day, assigned to the start date; requires ≥ 16 of 19 hours) the hourly 2 m RH minimum is taken, then ranked against the stored anchors with the dryness inversion (rank → 100 − rank + 4) so a low overnight minimum reads near 99. *Internal-consistency note: as written this 19-hour window is daytime-dominant in CONUS local time — 12Z is ~05 PDT/08 EDT (morning) and 06Z next day is ~23 PDT/02 EDT (night) — so it spans the ~21Z mid-afternoon RH minimum, meaning the window minimum captures the afternoon low and largely duplicates the Day-Min RH product rather than measuring overnight recovery. A genuine overnight-recovery window should run from evening to the following morning (roughly 00–03Z through 13–15Z, ~10–14 hours) and exclude afternoon hours; the stated window should be verified against the source.*
- **Reference:** RH — Bolton (1980); inverted ranking — Hyndman & Fan (1996); climatology — RTMA (De Pondeca et al. 2011).

### Surface Fire Weather Potential (weather-only, 0–100)
- **What it is:** A single 0–100 weather-only summary of how anomalous the fire-weather day is, blending six ingredient percentiles into one number. It is deliberately fuel-free — it says the atmosphere is unusual, not that fuels are ready to burn.
- **How it's computed:** For the 00–23Z window it computes six ingredient percentiles against their respective climatology anchors — day-min RH (dryness-inverted), day-max VPD, day-max sustained wind, day-max gust, day-max surface HDW (wind), day-max surface HDW (gust) — then takes their weighted mean with weights 0.22 (min-RH), 0.22 (VPD), 0.16 (wind), 0.16 (gust), 0.12 (HDW-wind), 0.12 (HDW-gust). A cell renders only if at least 4 ingredients are present and ≥ 0.70 of the total weight is available; the weighted mean is renormalized by the present weight. Output is a 0–100 percentile-blend, not a probability.
- **Reference:** Weighted percentile blend; component bases — Bolton (1980) (VPD/RH), Srock et al. (2018) (HDW), Hyndman & Fan (1996) (ranking); climatology — RTMA (De Pondeca et al. 2011).

### vs-record siblings (all-period baseline)
- **What it is:** For each anomaly product, a companion map that ranks the same forecast fold against the entire 2019–2026 record rather than just this time of year. It answers "is this extreme versus everything analyzed," not "versus this season."
- **How it's computed:** Identical forecast fold and identical empirical-CDF percentile (and dryness-inversion / weighted-blend) math as the seasonal product; only the reference distribution differs. The percentile anchors are the all-period empirical quantiles over every analyzed day 2019–2026 (~2695 samples per cell) instead of the ±7-day day-of-year window (~105 samples). The all-period set has no day-of-year axis, so a single distribution is used year-round.
- **Reference:** Empirical percentile ranking — Hyndman & Fan (1996), cf. Wilks (2019); component bases — Bolton (1980), Srock et al. (2018); climatology — RTMA (De Pondeca et al. 2011).

### Climatology reference maps (climo browser)
- **What it is:** A browser for the stored climatology itself — e.g. "what does p95 day-max VPD normally look like on July 15?" — shown directly in physical units with no forecast involved. It lets you inspect the very distribution the anomaly products rank against.
- **How it's computed:** Renders one stored empirical-percentile anchor grid (p05, p10, p25, p50, p75, p90, p95, or p99) for a chosen variable and target, in native display units (RH %, VPD kPa, wind/gust mph, surface-HDW kPa·m/s, joint-hours h). A day-of-year target reads the ±7-day seasonal store (~105 samples); a "record" target reads the all-period store (~2695 samples), which additionally carries an all-time max stat. No forecast and no ranking are performed — the anchors are the empirical quantiles of the 2019–2026 RTMA analysis distribution at each cell. The computed weather-only potential composite is not stored and has no reference grid.
- **Reference:** Displayed values are empirical quantiles — Hyndman & Fan (1996), cf. Wilks (2019); variable bases — Bolton (1980) (VPD/RH), Srock et al. (2018) (surface HDW); underlying analysis — RTMA (De Pondeca et al. 2011).

---

## Upper-air maps and standard (non-entraining) CAPE severe-weather composites

### Isobaric geopotential height + winds (850 / 700 / 500 / 300 / 250 / 200 mb)
- **What it is:** Constant-pressure charts showing the height of each pressure surface (a proxy for the large-scale ridge/trough pattern and jet structure) with wind barbs overlaid.
- **How it's computed:** Read directly from the HRRR model's isobaric fields: geopotential height on the requested level is drawn as a fill and as labeled contours, and the level's u/v wind components are drawn as barbs with speed = √(u² + v²). No parcel or kinematic math is applied.
- **Reference:** Native model field (isobaric height and wind).

### Isobaric temperature (per level)
- **What it is:** Air temperature on a constant-pressure surface, shown as a color fill under the height contours and wind barbs; highlights thermal advection, fronts, and mid-level warm/cold pools.
- **How it's computed:** Read directly from the HRRR model's temperature field on the requested pressure level and shaded. No derivation.
- **Reference:** Native model field (isobaric temperature).

### Isobaric relative humidity (per level)
- **What it is:** Relative humidity on a constant-pressure surface, indicating mid- and upper-level moisture, cloud layers, and dry slots.
- **How it's computed:** Read directly from the HRRR model's relative-humidity field on the requested pressure level and shaded. No derivation.
- **Reference:** Native model field (isobaric relative humidity).

### Isobaric absolute vorticity (per level)
- **What it is:** Absolute vorticity (earth plus relative vorticity) on a constant-pressure surface, used to locate vorticity maxima, shortwave troughs, and dynamic forcing for ascent (classically at 500 mb).
- **How it's computed:** Read directly from the HRRR model's absolute-vorticity field on the requested pressure level and shaded under the height contours and barbs; it is the model's own field, not recomputed from the wind.
- **Reference:** Native model field (isobaric absolute vorticity).

### SBCAPE / MLCAPE / MUCAPE (standard, non-entraining CAPE)
- **What it is:** Convective Available Potential Energy (J/kg): the total buoyant energy a rising parcel can convert to updraft kinetic energy. Larger CAPE means stronger potential updrafts. Three parcel choices are shown: surface-based (SB), 100-hPa mixed-layer (ML), and most-unstable (MU).
- **How it's computed:** Derived per grid column from the HRRR model's vertical profile (isobaric temperature, moisture converted to dewpoint, and geopotential height), with 2 m temperature/dewpoint and surface pressure prepended as the lowest level. A parcel is lifted with NO entrainment (undiluted pseudo-adiabatic ascent): dry-adiabatically to its LCL, then moist-adiabatically above. Parcel definition differs by type: SB uses the surface parcel, ML the mean θ/mixing-ratio of the lowest 100 hPa, MU the most-unstable parcel within the lowest 300 hPa. Using virtual temperature for both parcel (Tv_p) and environment (Tv_e), CAPE = g · ∫ over positive-buoyancy layers between LFC and equilibrium level of (Tv_p − Tv_e)/Tv_e dz, with g = 9.80665 m/s², by trapezoidal integration. This is the standard, non-entraining CAPE (distinct from the ECAPE lane).
- **Reference:** Moncrieff & Miller (1976).

### SBCIN / MLCIN / MUCIN (convective inhibition)
- **What it is:** Convective Inhibition (J/kg, negative): the energy that must be supplied to a parcel to overcome the stable layer below the LFC. Large-magnitude CIN caps convection; near-zero CIN means storms can initiate easily.
- **How it's computed:** The negative-area companion of the same non-entraining parcel lift used for CAPE. Over the negative-buoyancy layer(s) below the LFC (from the parcel's origin up to the LFC), CIN = g · ∫ (Tv_p − Tv_e)/Tv_e dz where Tv_p < Tv_e, using virtual temperature and g = 9.80665 m/s² (trapezoidal). Emitted for each of the SB, ML, and MU parcels.
- **Reference:** Moncrieff & Miller (1976).

### LCL height (sblcl)
- **What it is:** Height above ground of the Lifting Condensation Level for a surface-based parcel (meters AGL) — the approximate cloud-base height. Lower LCLs favor tornadoes.
- **How it's computed:** A by-product of the surface-based non-entraining parcel lift derived from the HRRR profile: the parcel is lifted dry-adiabatically until temperature and dewpoint converge (saturation), and that level's height above ground is reported.
- **Reference:** Moncrieff & Miller (1976) (parcel-theory basis; LCL is a standard parcel-lift diagnostic).

### 0–1 km / 0–3 km Storm-Relative Helicity
- **What it is:** Storm-Relative Helicity (m²/s²): a measure of the streamwise vorticity a storm inflow ingests over the 0–1 km or 0–3 km layer, quantifying the potential for a rotating (mesocyclonic) updraft. Higher values favor supercells and tornadoes.
- **How it's computed:** Derived from the HRRR profile's u/v winds and heights AGL. Relative to a storm-motion vector C, SRH = −∫ k · (V − C) × (dV/dz) dz over the layer, discretized as the sum over layers of [(u_{n+1} − C_u)(v_n − C_v) − (u_n − C_u)(v_{n+1} − C_v)], integrated to 1000 m and 3000 m AGL. Storm motion C is the Bunkers right-moving estimate (0–6 km mean wind plus a 7.5 m/s deviation perpendicular to, and right of, the surface-to-6 km shear vector). In the Southern Hemisphere the left-mover is used and the SRH sign flipped.
- **Reference:** Davies-Jones, Burgess & Foster (1990).

### 0–1 km / 0–6 km Bulk Shear
- **What it is:** The magnitude of the vector wind difference (bulk wind difference) across the 0–1 km or 0–6 km layer, in knots. Deep-layer (0–6 km) shear exceeding ~35–40 kt is a key discriminator for organized/supercell storms.
- **How it's computed:** Derived from the HRRR profile's u/v winds and heights AGL. Winds are interpolated to the bottom and top of the layer, and the result is the magnitude of their vector difference: shear = √((u_top − u_bot)² + (v_top − v_bot)²). Computed in m/s and converted to knots.
- **Reference:** Thompson et al. (2003).

### Significant Tornado Parameter, fixed-layer (stp_fixed)
- **What it is:** A multi-ingredient composite (dimensionless) blending instability, low cloud base, low-level rotation, and deep shear to flag environments supportive of significant (EF2+) tornadoes. Values ≳ 1 indicate an increasing significant-tornado threat.
- **How it's computed:** Derived from HRRR-profile ingredients as the product of four normalized/gated terms: STP = (SBCAPE/1500) × (LCL term) × (0–1 km SRH/150) × (0–6 km shear term). The LCL term is 1.0 for SB-LCL ≤ 1000 m, 0.0 for ≥ 2000 m, and (2000 − LCL)/1000 in between. The shear term is 0 below 12.5 m/s, capped at 1.5 once 0–6 km bulk shear reaches 30 m/s, else shear/20. CAPE and SRH terms are floored at 0. This is the fixed-layer (not effective-layer) form.
- **Reference:** Thompson et al. (2003); effective-layer refinements — Thompson et al. (2012).

### Supercell Composite Parameter, fixed-depth proxy (scp_mu_0_3km_0_6km_proxy)
- **What it is:** A composite (dimensionless) combining most-unstable buoyancy, low-level rotation, and deep shear to flag supercell-supporting environments. This is an experimental fixed-depth proxy, not the operational effective-layer SCP.
- **How it's computed:** Derived from HRRR-profile ingredients as SCP = (MUCAPE/1000) × (0–3 km SRH/50) × (shear term). The shear term (using 0–6 km bulk shear) is 0 below 10 m/s, 1.0 above 20 m/s, else shear/20; CAPE and SRH terms floored at 0. The operational SCP uses effective-layer SRH and effective bulk wind difference; here 0–3 km SRH and 0–6 km shear substitute, hence the explicit proxy label.
- **Reference:** Thompson et al. (2003); effective-layer form — Thompson et al. (2012).

### Energy-Helicity Index, 0–1 km / 0–3 km
- **What it is:** A dimensionless composite of buoyancy and low-level rotation that scales the tornado/supercell threat; higher EHI indicates environments increasingly favorable for rotating storms and tornadoes.
- **How it's computed:** Derived from HRRR-profile ingredients as EHI = (CAPE × SRH) / 160000, using standard (non-entraining) surface-based CAPE with 0–1 km SRH for ehi_0_1km and 0–3 km SRH for ehi_0_3km. The 160000 constant is the standard normalization.
- **Reference:** Hart & Korotky (1991).

### Storm motion (Bunkers right-mover)
- **What it is:** The estimated motion vector of a right-moving supercell. It is the reference frame that makes helicity "storm-relative," and underpins every SRH-based product in this family.
- **How it's computed:** Derived from the HRRR profile's u/v winds and heights via the Bunkers internal-dynamics method: take the 0–6 km depth-weighted mean wind, then add a 7.5 m/s deviation perpendicular to the 0–6 km shear vector (built from the 0–0.5 km and 5.5–6 km mean winds). Adding the deviation to the right of the shear gives the right-mover; to the left gives the left-mover (used in the Southern Hemisphere). This vector is the storm-motion reference C in the SRH and effective-layer calculations.
- **Reference:** Bunkers et al. (2000).

---

## ECAPE suite & PyroCumulonimbus Firepower Threshold (PFT)

### SB / ML / MU Entraining CAPE (ECAPE)
- **What it is:** Entraining convective available potential energy: the buoyant energy left to a rising parcel after its buoyancy is diluted by lateral entrainment of drier environmental air, for surface-based (SB), 100-hPa mixed-layer (ML), and most-unstable (MU) source parcels. It is always ≤ the ordinary (non-entraining) CAPE and is a more realistic measure of updraft potential.
- **How it's computed:** Solved per column from the HRRR model's vertical temperature/moisture/wind profile (with a 2 m/10 m surface level prepended). An undiluted parcel is first lifted (pseudoadiabatic) to obtain CAPE, LFC, and EL. The storm-relative inflow wind VSR (0–1 km, using Bunkers right-moving storm motion), the normalized CAPE (NCAPE), and the storm depth H = EL − parcel origin set a fractional entrainment rate diagnosed automatically from the sounding: ε = 2(1 − Ẽ)/(Ẽ + Ñ)/H, with Ẽ = ECAPE_A/CAPE − VSR̃² (ECAPE_A the analytic entraining CAPE, VSR̃ = VSR/√(2·CAPE)) and Ñ = NCAPE/CAPE. The parcel is re-lifted with that ε (entrainment terms −ε·(T_parcel − T_env) and −ε·(q_parcel − q_env) added to the lapse-rate and moisture equations), and ECAPE is the positive-buoyancy integral of the diluted path. SB/ML/MU differ only in the source parcel.
- **Reference:** Peters et al. (2020, 2022, 2023).

### SB NCAPE (normalized / buoyancy-dilution CAPE)
- **What it is:** Normalized CAPE, i.e. the buoyancy-dilution potential of the surface-based parcel: a measure of how vulnerable an updraft's buoyancy is to entrainment. Larger NCAPE means a given amount of CAPE is more easily eroded by mixing.
- **How it's computed:** From the HRRR profile, NCAPE = ∫ from LFC to EL of −(g/(c_pd·T))·(MSE̅ − MSE*) dz, where MSE* is the saturation moist static energy at each level and MSE̅ is the surface-to-level vertical mean of the environmental moist static energy; evaluated by the trapezoidal rule between the surface-based parcel's LFC and EL. It is a by-product of the SB entraining-parcel solve and sets the automatic entrainment rate in ECAPE.
- **Reference:** Peters et al. (2020, 2023).

### SB / ML ECAPE CIN
- **What it is:** The convective inhibition experienced by the entraining parcel: the negative-buoyancy energy that must be overcome before the diluted parcel can freely convect, for SB and ML source parcels.
- **How it's computed:** The CIN diagnostic of the same entraining (diluted-parcel) ascent used for ECAPE: the magnitude of the negative-buoyancy integral below the LFC along the entrained parcel path through the HRRR profile. It is returned alongside ECAPE from the per-column solve.
- **Reference:** Peters et al. (2023, 2020).

### SB / ML / MU ECAPE-to-derived-CAPE ratio (entrainment survival fraction)
- **What it is:** The fraction of a parcel's buoyant energy that survives entrainment: entraining CAPE divided by the corresponding non-entraining CAPE for the same parcel. Values near 1 mean entrainment barely erodes the updraft; small values mean strong dilution by dry-air mixing.
- **How it's computed:** Elementwise ECAPE / CAPE for the matched SB, ML, or MU parcel, both taken from a per-column solve over the HRRR profile. Computed only where the denominator CAPE is at least 100 J/kg (otherwise flagged as no-data). This is Peters' Ẽ = ECAPE/CAPE survival ratio expressed as a map.
- **Reference:** Peters et al. (2023, 2020).

### SB / ML / MU ECAPE-to-native-CAPE ratio
- **What it is:** The same entrainment survival-fraction idea, but comparing the entraining CAPE against the HRRR model's own CAPE for that parcel class rather than a recomputed CAPE.
- **How it's computed:** Elementwise (computed ECAPE) / (HRRR native SB/ML/MU CAPE field), matched by parcel type, with the denominator gated at 100 J/kg or more. Emitted only where the corresponding native CAPE field is present.
- **Reference:** Peters et al. (2023, 2020).

### ECAPE-weighted Supercell Composite Parameter (ecape_scp, experimental)
- **What it is:** A supercell composite parameter built from entraining (rather than ordinary) most-unstable CAPE: a single index flagging environments favorable for supercells. Experimental.
- **How it's computed:** SCP = (MU ECAPE / 1000 J/kg) × (0–3 km SRH / 50 m²/s²) × shear-term, where the shear-term uses the 0–6 km bulk wind difference (EBWD): 0 below 10 m/s, EBWD/20 between 10 and 20 m/s, capped at 1 above 20 m/s. This is the effective-layer SCP form with MU ECAPE substituted for MUCAPE; SRH and bulk shear come from the HRRR wind profile.
- **Reference:** SCP form — Thompson, Mead & Edwards (2007); ECAPE substitution — Peters et al. (2023).

### ECAPE-weighted Energy-Helicity Index 0–1 km / 0–3 km (experimental)
- **What it is:** The energy-helicity index computed with entraining CAPE: it combines low-level storm-relative helicity with surface-based entraining CAPE to gauge supercell/tornado potential. Experimental, offered at two SRH depths.
- **How it's computed:** EHI = (SB ECAPE × SRH) / 160000, with SRH taken over the 0–1 km or 0–3 km layer of the HRRR wind profile. This is the standard EHI normalization with SB ECAPE replacing ordinary CAPE.
- **Reference:** EHI — Rasmussen (2003), after Hart & Korotky (1991); ECAPE substitution — Peters et al. (2023).

### ECAPE-weighted Significant Tornado Parameter (ecape_stp, experimental)
- **What it is:** The significant tornado parameter in its effective-layer (with-CIN) form, computed from entraining mixed-layer CAPE and CIN: an experimental composite for significant-tornado environments.
- **How it's computed:** STP = (ML ECAPE / 1500) × LCL-term × (0–1 km SRH / 150) × shear-term × CIN-term. LCL-term = 1 for classic ML-parcel LCL ≤ 1000 m, 0 at ≥ 2000 m, linear between; shear-term uses the 0–6 km bulk wind difference = 0 below 12.5 m/s, EBWD/20 up to 30 m/s, capped at 1.5; CIN-term = clamp((200 + ML ECIN)/150, 0, 1). It uses ML ECAPE and ML ECIN with the classic (non-entraining) ML-parcel LCL, and HRRR 0–1 km SRH and 0–6 km shear.
- **Reference:** STP effective-layer form — Thompson et al. (2012); ECAPE substitution — Peters et al. (2023).

### PyroCumulonimbus Firepower Threshold (pft_gw)
- **What it is:** The minimum total sensible-heat flux (in gigawatts) a fire must feed into the base of its smoke plume for the plume to reach free moist convection deep enough (cloud top colder than −20 °C) to form a pyrocumulonimbus, given that atmospheric column. Lower PFT means a more favorable atmosphere for pyroCb. Experimental (this is PFT1).
- **How it's computed:** Solved per HRRR column via Tory & Kepert (2021) Eq. 25: PFT = 397.3 · ρ0 · z_fc² · U_ML · Δθ_fc (watts), reported in GW, where 397.3 = π·Cpd·(β′/(1 + a′·β′))² with β′ = 0.4, a′ = 0.32. Steps: (1) build a height-weighted entrained mixed layer (θ_ML, q_ML) grown level-by-level until its own LCL falls inside it; (2) march the saturation-point curve θ_SP = (1 + β)·θ_ML, q_SP = q_ML + β·φ·θ_ML (φ = 6.67e−5 kg/kg/K, the Luderer 15 K per g/kg fire-moisture ratio), solving each SP point via Tory-Thurston-Kepert (2018) Eqs. 14–17; (3) find the marginal buoyancy parameter β* whose saturated pseudoadiabat stays at least 0.5 K (virtual-temperature) warmer than the environment all the way up to the −20 °C parcel level, by a coarse scan plus bisection; (4) z_fc = height AGL of P_SP(β*), Δθ_fc = β*·θ_ML, U_ML = trapezoid vector-mean wind over 0 to z_fc, and ρ0/Pc from Eqs. 26/28. Columns with no freely-convecting path within the scan get a > 1024 GW sentinel. The entraining PFT2 variant is not implemented (its constant is unpublished).
- **Reference:** Tory & Kepert (2021); saturation-point curve — Tory, Thurston & Kepert (2018); fire-moisture ratio — Luderer, Trentmann & Andreae (2009).

### PFT free-convection height (pft_zfc)
- **What it is:** The height above ground of the marginal free-convection level in the PFT solution: how high the smoke plume must rise before it can convect freely. A diagnostic component of the firepower threshold.
- **How it's computed:** z_fc is the height AGL, by log-pressure interpolation of the HRRR height profile, of the saturation-point pressure P_SP evaluated at the marginal buoyancy parameter β* found in the PFT solve.
- **Reference:** Tory & Kepert (2021); Tory, Thurston & Kepert (2018).

### PFT required plume warming (pft_dtheta_fc)
- **What it is:** The fire-induced potential-temperature excess the plume must retain at the free-convection level to make a pyroCb: the plume warmth the fire has to supply. A component of the firepower threshold.
- **How it's computed:** Δθ_fc = β* · θ_ML, where β* is the marginal buoyancy parameter and θ_ML the entrained mixed-layer potential temperature from the PFT column solve over the HRRR profile.
- **Reference:** Tory & Kepert (2021).

### PFT mixed-layer wind (pft_uml)
- **What it is:** The mean surface-to-z_fc wind speed used in the firepower formula: stronger winds bend the plume over and raise the firepower a fire needs to reach free convection. A component of the firepower threshold.
- **How it's computed:** U_ML is the magnitude of the trapezoidal vector-mean of the u and v wind components over the surface-to-z_fc layer of the HRRR wind profile (the vector mean is taken first, then its magnitude, so directional shear within the layer reduces U_ML).
- **Reference:** Tory & Kepert (2021).

---

## Point products + fire perimeters

### Point Meteogram
- **What it is:** A multi-panel time series of conditions at a single point (the nearest model grid cell) across every stored hour of one forecast run: temperature/dewpoint, relative humidity, VPD with a surface Hot-Dry-Windy proxy, wind and gusts, precipitation, fuels (ERC and 10-h dead-fuel moisture), and near-surface smoke. It lets a forecaster read how one location evolves through the forecast, with critical fire-weather hours shaded and day/night and climatological reference lines drawn in.
- **How it's computed:** Samples the nearest grid cell (minimum squared equirectangular distance) at each stored forecast hour. Native fields are read straight from the model column: 2 m temperature and dewpoint (K → °F), 2 m RH, 10 m u/v winds and gust (m/s → mph, sustained speed = √(u² + v²)), ERC, 10-h dead-fuel moisture, KBDI, 8 m smoke (kg/m³ → µg/m³), and run-total precip. Derived per hour: saturation vapor pressure via the Bolton/Magnus form e_s(T_C) = 6.112·exp(17.67·T_C/(T_C + 243.5)) hPa; VPD = (e_s(T) − e_s(Td))·0.1, clamped ≥ 0 (kPa); surface Hot-Dry-Windy proxy HDW = VPD × 10 m wind speed (m/s), emitted as two series (sustained and gust); step precipitation = successive differences of the run-total accumulation / 25.4 (mm → in). Hours meeting the joint thresholds (RH ≤ 20 % and wind ≥ 20 mph, or RH ≤ 15 % and gust ≥ 25 mph) are shaded across all panels. Dashed "normal here today" reference lines (VPD p50/p90/p99, gust p90/p99, min-RH p50/p10) are percentiles from a seasonal RTMA-analysis climatology keyed to the run's day-of-year; night shading uses the NOAA solar sunrise/sunset formula at the cell.
- **Reference:** Saturation vapor pressure / VPD — Bolton (1980); Hot-Dry-Windy Index — Srock et al. (2018).

### Daily Outlook Card
- **What it is:** A shareable card with one column per local calendar day (or per fixed 1/3/6-hour bucket) showing each day's HI and LO for a chosen stored variable as colored number strips, plus a wind row and a precipitation row. A quick at-a-glance, day-by-day outlook for a single point.
- **How it's computed:** Samples the requested 2-D variable at the nearest grid cell for each stored hour, converts the valid time to local time (UTC offset), and buckets the samples by local calendar day (or by the requested fixed step). Per bucket HI = max and LO = min of the sampled values; temperature uses a fixed absolute color scale while other variables auto-normalize across the card. The wind row shows the vector-mean 10 m wind direction and the bucket-maximum sustained speed; the precipitation row chains run-total accumulation differences across buckets (/ 25.4, mm → in). Partial buckets (fewer than ~3/4 of the samples a full bucket would hold at the model's hour spacing) are dropped, and columns ≥ 8 local days past init are flagged as extended-range. Units auto-convert (K → °F; m/s → mph for wind/gust variables).
- **Reference:** Native model field; per-local-day (or per-fixed-bucket) max/min with wind vector-averaging and precipitation differencing only; no external derivation.

### Vertical Cross Section
- **What it is:** A vertical slice of the atmosphere along a user-drawn A→B line: a smooth color fill of temperature, relative humidity, or wind speed on a log-pressure vertical axis (1000 hPa at the bottom, 150 hPa at the top), with a solid terrain silhouette and wind barbs. It shows how atmospheric structure changes along a transect, e.g. across a mountain range or a front.
- **How it's computed:** 200 columns are sampled along the straight lat/lon line between the endpoints; both lat/lon and fractional grid indices interpolate linearly between the two endpoint cells (at most ~2-cell drift mid-path on the near-uniform grid). Each column is bilinearly sampled from the run's isobaric volumes (temperature, u/v; 100–1000 hPa at 25-hPa spacing). Temperature is converted K → °F; wind speed = √(u² + v²) · 2.23694 (m/s → mph); RH is derived per level via the Magnus/Bolton relation RH = 100 · e_s(Td)/e_s(T) clamped to 0–100, with e_s(T_C) = 6.112·exp(17.67·T_C/(T_C + 243.5)) hPa. The vertical axis is log-p, each fill cell spanning geometric-mean (log-p midpoint) level boundaries. The terrain silhouette is the model surface pressure along the path (Pa → hPa) on the same log-p axis; barbs are true wind barbs, decimated and skipped below ground. Path distances use the haversine great-circle formula.
- **Reference:** RH derivation (saturation vapor pressure) — Bolton (1980); temperature and wind fills are native model isobaric fields; the log-p vertical coordinate is the standard skew-T/log-p pressure axis.

### Point Sounding (skew-T)
- **What it is:** A full skew-T/log-p thermodynamic diagram at a point, with a wind-barb stave, a hodograph, an entraining-CAPE (ECAPE) block, a locator map, and a parameter table. It is the standard tool for reading a single column's stability, moisture, and wind profile, including storm-motion vectors on the hodograph.
- **How it's computed:** Builds the vertical profile at the nearest grid cell from the run's isobaric volumes (temperature, dewpoint, u, v, height) with a 2 m/10 m surface level prepended from the surface fields (surface pressure, 2 m temperature/dewpoint, 10 m u/v, orography). Below-ground levels (p ≥ surface pressure) are dropped, dewpoint is clamped to temperature, and descending-pressure order is preserved. The assembled column is passed to a vendored SHARPpy-derived engine, which lifts surface-based / mixed-layer / most-unstable parcels (CAPE/CIN, LCL/LFC/EL), and computes 0–1/0–3/0–6 km bulk shear, 0–1/0–3 km storm-relative helicity, lapse rates (0–3 km, 3–6 km, 700–500, 850–500), precipitable water, DCAPE, K-index, total totals, freezing and wet-bulb-zero levels, and storm-motion vectors — Bunkers right- and left-moving, and Corfidi upshear/downshear. Single-column entraining CAPE and NCAPE (SB/ML/MU ECAPE) are computed via an entraining-CAPE kernel (Bunkers right-moving storm motion, automatic entrainment; flagged experimental). The result is rendered as the composed skew-T image.
- **Reference:** Skew-T/log-p and the hodograph are standard diagrams; storm motions — Bunkers et al. (2000), Corfidi (2003); parameter suite — Blumberg et al. (2017) (SHARPpy); entraining CAPE — Peters et al. (2023).

### Fire Perimeters
- **What it is:** The current mapped active-fire perimeter polygon overlaid on the map — the interagency-mapped outer boundary of an active wildfire, labeled with incident name, mapped acreage, origin state, and last-updated time.
- **How it's computed:** Not a computed scientific product. For display, only the single largest outer ring of each incident is kept and decimated toward ~240 vertices; the set is limited to larger active fires and rendered as the mapped polygon in geographic coordinates.
- **Reference:** Not a scientific derivation; operational interagency-mapped active-fire perimeter polygons displayed as-is.

---

## References

- Ambaum, M. H. P. (2020). Accurate, simple equation for saturated vapour pressure over water and ice. *Quarterly Journal of the Royal Meteorological Society*, 146(733), 4252–4258. doi:10.1002/qj.3899
- Blumberg, W. G., K. T. Halbert, T. A. Supinie, P. T. Marsh, R. L. Thompson, and J. A. Hart (2017). SHARPpy: An Open-Source Sounding Analysis Toolkit for the Atmospheric Sciences. *Bulletin of the American Meteorological Society*, 98(8), 1625–1636. doi:10.1175/BAMS-D-15-00309.1
- Bolton, D. (1980). The Computation of Equivalent Potential Temperature. *Monthly Weather Review*, 108(7), 1046–1053. doi:10.1175/1520-0493(1980)108<1046:TCOEPT>2.0.CO;2
- Bradshaw, L. S., J. E. Deeming, R. E. Burgan, and J. D. Cohen (comps.) (1983). *The 1978 National Fire-Danger Rating System: Technical Documentation.* USDA Forest Service General Technical Report INT-169, Intermountain Forest and Range Experiment Station, Ogden, UT.
- Bunkers, M. J., B. A. Klimowski, J. W. Zeitler, R. L. Thompson, and M. L. Weisman (2000). Predicting supercell motion using a new hodograph technique. *Weather and Forecasting*, 15(1), 61–79. doi:10.1175/1520-0434(2000)015<0061:PSMUAN>2.0.CO;2
- Corfidi, S. F. (2003). Cold Pools and MCS Propagation: Forecasting the Motion of Downwind-Developing MCSs. *Weather and Forecasting*, 18(6), 997–1017. doi:10.1175/1520-0434(2003)018<0997:CPAMPF>2.0.CO;2
- Davies-Jones, R., D. Burgess, and M. Foster (1990). Test of helicity as a tornado forecast parameter. *Preprints, 16th Conference on Severe Local Storms*, Kananaskis Park, AB, Canada, American Meteorological Society, 588–592.
- Deeming, J. E., R. E. Burgan, and J. D. Cohen (1977). *The National Fire-Danger Rating System — 1978.* USDA Forest Service General Technical Report INT-39, Intermountain Forest and Range Experiment Station, Ogden, UT.
- De Pondeca, M. S. F. V., et al. (2011). The Real-Time Mesoscale Analysis at NOAA's National Centers for Environmental Prediction: Current Status and Development. *Weather and Forecasting*, 26(5), 593–612. doi:10.1175/WAF-D-10-05037.1
- Fosberg, M. A. (1978). Weather in wildland fire management: the fire weather index. *Proceedings of the Conference on Sierra Nevada Meteorology*, South Lake Tahoe, CA, 19–21 June 1978, American Meteorological Society, 1–4.
- Hart, J. A., and W. D. Korotky (1991). *The SHARP Workstation v1.50 Users Guide.* NOAA/National Weather Service, 30 pp. [NWS Eastern Region Headquarters, Bohemia, NY.]
- Hyndman, R. J., and Y. Fan (1996). Sample Quantiles in Statistical Packages. *The American Statistician*, 50(4), 361–365. doi:10.1080/00031305.1996.10473566
- Keetch, J. J., and G. M. Byram (1968). *A Drought Index for Forest Fire Control.* USDA Forest Service Research Paper SE-38, Southeastern Forest Experiment Station, Asheville, NC (revised 1988).
- Luderer, G., J. Trentmann, and M. O. Andreae (2009). A new look at the role of fire-released moisture on the dynamics of atmospheric pyro-convection. *International Journal of Wildland Fire*, 18(5), 554–562. doi:10.1071/WF07035
- Moncrieff, M. W., and M. J. Miller (1976). The dynamics and simulation of tropical cumulonimbus and squall lines. *Quarterly Journal of the Royal Meteorological Society*, 102(432), 373–394. doi:10.1002/qj.49710243208
- Peters, J. M., C. J. Nowotarski, J. P. Mulholland, and R. L. Thompson (2020). The Influences of Effective Inflow Layer Streamwise Vorticity and Storm-Relative Flow on Supercell Updraft Properties. *Journal of the Atmospheric Sciences*, 77(9), 3033–3057. doi:10.1175/JAS-D-19-0355.1
- Peters, J. M., J. P. Mulholland, and D. R. Chavas (2022). Generalized Lapse Rate Formulas for Use in Entraining CAPE Calculations. *Journal of the Atmospheric Sciences*, 79(3), 815–836. doi:10.1175/JAS-D-21-0118.1
- Peters, J. M., D. R. Chavas, C.-Y. Su, H. Morrison, and B. E. Coffer (2023). An Analytic Formula for Entraining CAPE in Midlatitude Storm Environments. *Journal of the Atmospheric Sciences*, 80(9), 2165–2186. doi:10.1175/JAS-D-23-0003.1
- Rasmussen, E. N. (2003). Refined Supercell and Tornado Forecast Parameters. *Weather and Forecasting*, 18(3), 530–535. doi:10.1175/1520-0434(2003)18<530:RSATFP>2.0.CO;2
- Rollins, M. G. (2009). LANDFIRE: a nationally consistent vegetation, wildland fire, and fuel assessment. *International Journal of Wildland Fire*, 18(3), 235–249. doi:10.1071/WF08088
- Seager, R., A. Hooks, A. P. Williams, B. I. Cook, J. Nakamura, and N. Henderson (2015). Climatology, Variability, and Trends in the U.S. Vapor Pressure Deficit, an Important Fire-Related Meteorological Quantity. *Journal of Applied Meteorology and Climatology*, 54(6), 1121–1141. doi:10.1175/JAMC-D-14-0321.1
- Srock, A. F., J. J. Charney, B. E. Potter, and S. L. Goodrick (2018). The Hot-Dry-Windy Index: A New Fire Weather Index. *Atmosphere*, 9(7), 279. doi:10.3390/atmos9070279
- Thompson, R. L., R. Edwards, J. A. Hart, K. L. Elmore, and P. Markowski (2003). Close Proximity Soundings within Supercell Environments Obtained from the Rapid Update Cycle. *Weather and Forecasting*, 18(6), 1243–1261. doi:10.1175/1520-0434(2003)018<1243:CPSWSE>2.0.CO;2
- Thompson, R. L., C. M. Mead, and R. Edwards (2007). Effective Storm-Relative Helicity and Bulk Shear in Supercell Thunderstorm Environments. *Weather and Forecasting*, 22(1), 102–115. doi:10.1175/WAF969.1
- Thompson, R. L., B. T. Smith, J. S. Grams, A. R. Dean, and C. Broyles (2012). Convective Modes for Significant Severe Thunderstorms in the Contiguous United States. Part II: Supercell and QLCS Tornado Environments. *Weather and Forecasting*, 27(5), 1136–1154. doi:10.1175/WAF-D-11-00116.1
- Tory, K. J., W. Thurston, and J. D. Kepert (2018). Thermodynamics of Pyrocumulus: A Conceptual Study. *Monthly Weather Review*, 146(8), 2579–2598. doi:10.1175/MWR-D-17-0377.1
- Tory, K. J., and J. D. Kepert (2021). Pyrocumulonimbus Firepower Threshold: Assessing the Atmospheric Potential for pyroCb. *Weather and Forecasting*, 36(2), 439–456. doi:10.1175/WAF-D-20-0027.1
- Wilks, D. S. (2019). *Statistical Methods in the Atmospheric Sciences*, 4th ed. Elsevier.
