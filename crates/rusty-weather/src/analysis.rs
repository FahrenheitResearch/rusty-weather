//! Numbers-only briefing text for a machine-written sounding discussion.
//!
//! This module reduces `/api/sounding?format=json` to the block a language model
//! is actually good at reasoning over, and stops there. It performs no network
//! I/O and holds no credentials: `/api/analysis` serves what it produces, and the
//! call out to a provider is deliberately NOT wired up yet.
//!
//! Two decisions worth keeping:
//!
//! * **No image, and not the raw levels either.** A 34-level table is ~1.5k
//!   tokens of digits that a model then does arithmetic on, badly — averaging
//!   layers, losing which way pressure runs. `sharprs` has already computed what
//!   a forecaster reads, so the block carries the reductions plus a handful of
//!   mandatory levels for vertical structure.
//! * **The characterizations are computed HERE.** Inversions, dry and moist
//!   layers, hodograph curvature: all measured in Rust and stated as facts, so
//!   the model's job is prose and never calculation. That removes most of the
//!   room it has to be confidently wrong.
//!
//! Formatting rules the cache and the prose both depend on: units in every
//! label, a FIXED field order so identical inputs give identical text, and
//! missing values written `--` rather than `0` — an unavailable CAPE and a zero
//! CAPE are different claims.

use serde_json::Value;

/// Default provider model. Overridable per request; the slug must match the
/// provider's catalogue.
pub const DEFAULT_MODEL: &str = "moonshotai/kimi-k3";

/// Pressure levels always quoted, so vertical structure can be described without
/// shipping every level.
const MANDATORY_LEVELS_HPA: &[f64] = &[925.0, 850.0, 700.0, 500.0, 300.0, 200.0];

/// Indices quoted in the briefing, in this order. Left is the JSON key, right is
/// the label with its unit.
const INDEX_FIELDS: &[(&str, &str)] = &[
    ("sbcape_j_kg", "SBCAPE J/kg"),
    ("sbcin_j_kg", "SBCIN J/kg"),
    ("mlcape_j_kg", "MLCAPE J/kg"),
    ("mlcin_j_kg", "MLCIN J/kg"),
    ("mucape_j_kg", "MUCAPE J/kg"),
    ("mucin_j_kg", "MUCIN J/kg"),
    ("dcape_j_kg", "DCAPE J/kg"),
    ("lcl_m_agl", "LCL m AGL"),
    ("lfc_m_agl", "LFC m AGL"),
    ("el_m_agl", "EL m AGL"),
    ("lapse_0_3km_c_km", "lapse 0-3 km C/km"),
    ("lapse_3_6km_c_km", "lapse 3-6 km C/km"),
    ("lapse_700_500_c_km", "lapse 700-500 C/km"),
    ("lapse_850_500_c_km", "lapse 850-500 C/km"),
    ("shear_0_1km_kt", "shear 0-1 km kt"),
    ("shear_0_3km_kt", "shear 0-3 km kt"),
    ("shear_0_6km_kt", "shear 0-6 km kt"),
    ("srh_0_1km_m2_s2", "SRH 0-1 km m2/s2"),
    ("srh_0_3km_m2_s2", "SRH 0-3 km m2/s2"),
    ("pwat_in", "PWAT in"),
    ("k_index", "K index"),
    ("freezing_level_m_agl", "freezing level m AGL"),
    ("wet_bulb_zero_m_agl", "wet-bulb zero m AGL"),
];

/// Render one index value, or `--` when the store or the engine had none.
fn number(value: Option<&Value>) -> String {
    match value {
        Some(Value::Number(number)) => {
            let as_f64 = number.as_f64().unwrap_or(f64::NAN);
            if !as_f64.is_finite() {
                "--".to_string()
            } else if (as_f64 - as_f64.round()).abs() < 1.0e-9 {
                format!("{}", as_f64.round() as i64)
            } else {
                format!("{as_f64:.1}")
            }
        }
        _ => "--".to_string(),
    }
}

fn floats(profile: &Value, key: &str) -> Vec<f64> {
    profile
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .map(|value| value.as_f64().unwrap_or(f64::NAN))
                .collect()
        })
        .unwrap_or_default()
}

/// Relative humidity from temperature and dewpoint (Magnus), percent.
fn relative_humidity(t_c: f64, td_c: f64) -> f64 {
    let sat = |t: f64| 6.112 * ((17.67 * t) / (t + 243.5)).exp();
    (100.0 * sat(td_c) / sat(t_c)).clamp(0.0, 100.0)
}

/// Layer-mean RH between two pressures, `None` when the layer has no levels.
fn layer_mean_rh(pressure: &[f64], t: &[f64], td: &[f64], top: f64, bottom: f64) -> Option<f64> {
    let mut total = 0.0;
    let mut count = 0usize;
    for index in 0..pressure.len().min(t.len()).min(td.len()) {
        let p = pressure[index];
        if !(p.is_finite() && (top..=bottom).contains(&p)) {
            continue;
        }
        if !(t[index].is_finite() && td[index].is_finite()) {
            continue;
        }
        total += relative_humidity(t[index], td[index]);
        count += 1;
    }
    (count > 0).then(|| total / count as f64)
}

/// The lowest temperature INVERSION: a layer where temperature rises with
/// height. Stated as a fact so the model never has to infer it from levels.
fn lowest_inversion(pressure: &[f64], t: &[f64]) -> Option<(f64, f64, f64)> {
    let levels = pressure.len().min(t.len());
    let mut start: Option<usize> = None;
    for index in 1..levels {
        let (p0, p1) = (pressure[index - 1], pressure[index]);
        let (t0, t1) = (t[index - 1], t[index]);
        if !(p0.is_finite() && p1.is_finite() && t0.is_finite() && t1.is_finite()) {
            continue;
        }
        // Arrays run surface-first with pressure DECREASING, so "with height"
        // means the next index.
        if t1 > t0 + 0.1 {
            if start.is_none() {
                start = Some(index - 1);
            }
        } else if let Some(from) = start.take() {
            let gain = t[index - 1] - t[from];
            if gain >= 0.5 {
                return Some((pressure[from], pressure[index - 1], gain));
            }
        }
    }
    if let Some(from) = start {
        let last = levels - 1;
        let gain = t[last] - t[from];
        if gain >= 0.5 {
            return Some((pressure[from], pressure[last], gain));
        }
    }
    None
}

/// Hodograph turning between the surface and ~3 km, as a signed degrees figure:
/// positive is clockwise (veering).
fn hodograph_turning(pressure: &[f64], height: &[f64], u: &[f64], v: &[f64]) -> Option<f64> {
    let levels = pressure.len().min(height.len()).min(u.len()).min(v.len());
    if levels < 2 {
        return None;
    }
    let surface_height = height.first().copied()?;
    let bearing = |u: f64, v: f64| (-u).atan2(-v).to_degrees().rem_euclid(360.0);
    let mut first: Option<f64> = None;
    let mut last: Option<f64> = None;
    for index in 0..levels {
        if !(height[index].is_finite() && u[index].is_finite() && v[index].is_finite()) {
            continue;
        }
        let agl = height[index] - surface_height;
        if agl < 0.0 || agl > 3000.0 {
            continue;
        }
        let bearing = bearing(u[index], v[index]);
        if first.is_none() {
            first = Some(bearing);
        }
        last = Some(bearing);
    }
    let (first, last) = (first?, last?);
    let mut turn = last - first;
    while turn > 180.0 {
        turn -= 360.0;
    }
    while turn < -180.0 {
        turn += 360.0;
    }
    Some(turn)
}

/// Build the briefing block from a `/api/sounding?format=json` payload.
pub fn briefing_from_sounding(data: &Value) -> String {
    let profile = data.get("profile").cloned().unwrap_or(Value::Null);
    let indices = data.get("indices").cloned().unwrap_or(Value::Null);
    let pressure = floats(&profile, "pressure_hpa");
    let height = floats(&profile, "height_m_msl");
    let temperature = floats(&profile, "temperature_c");
    let dewpoint = floats(&profile, "dewpoint_c");
    let u = floats(&profile, "u_ms");
    let v = floats(&profile, "v_ms");

    let mut out = String::with_capacity(2048);
    out.push_str("SOUNDING BRIEFING (all values measured, none inferred)\n");
    out.push_str(&format!(
        "model: {}   run: {}   forecast hour: F{:03}   valid: {}\n",
        data.get("model").and_then(Value::as_str).unwrap_or("--"),
        data.get("run").and_then(Value::as_str).unwrap_or("--"),
        data.get("hour").and_then(Value::as_u64).unwrap_or(0),
        data.get("valid").and_then(Value::as_str).unwrap_or("--"),
    ));
    out.push_str(&format!(
        "location: {}\n",
        data.get("nearest_place")
            .and_then(Value::as_str)
            .unwrap_or("--")
    ));

    out.push_str("\nINDICES\n");
    for (key, label) in INDEX_FIELDS {
        out.push_str(&format!(
            "  {label}: {}\n",
            number(indices.get(key))
        ));
    }

    out.push_str("\nSURFACE\n");
    if let (Some(&p), Some(&t), Some(&td)) = (pressure.first(), temperature.first(), dewpoint.first())
    {
        out.push_str(&format!(
            "  {p:.0} hPa: T {t:.1} C, Td {td:.1} C, RH {:.0}%\n",
            relative_humidity(t, td)
        ));
    } else {
        out.push_str("  --\n");
    }

    out.push_str("\nMANDATORY LEVELS (nearest stored level)\n");
    for &target in MANDATORY_LEVELS_HPA {
        let nearest = pressure
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_finite())
            .min_by(|(_, a), (_, b)| {
                (*a - target)
                    .abs()
                    .total_cmp(&(*b - target).abs())
            });
        match nearest {
            Some((index, &p)) if (p - target).abs() < 30.0 => {
                let t = temperature.get(index).copied().unwrap_or(f64::NAN);
                let td = dewpoint.get(index).copied().unwrap_or(f64::NAN);
                let wind = match (u.get(index), v.get(index)) {
                    (Some(&u), Some(&v)) if u.is_finite() && v.is_finite() => format!(
                        "{:.0}/{:.0} kt",
                        (-u).atan2(-v).to_degrees().rem_euclid(360.0),
                        u.hypot(v) * 1.943_844
                    ),
                    _ => "--".to_string(),
                };
                out.push_str(&format!(
                    "  {target:.0} hPa: T {t:.1} C, Td {td:.1} C, RH {:.0}%, wind {wind}\n",
                    relative_humidity(t, td)
                ));
            }
            _ => out.push_str(&format!("  {target:.0} hPa: not stored\n")),
        }
    }

    out.push_str("\nSTRUCTURE (computed, not for you to derive)\n");
    match lowest_inversion(&pressure, &temperature) {
        Some((base, top, gain)) => out.push_str(&format!(
            "  lowest inversion: {base:.0}-{top:.0} hPa, +{gain:.1} C\n"
        )),
        None => out.push_str("  lowest inversion: none\n"),
    }
    for (label, top, bottom) in [
        ("surface-850 hPa", 850.0, 1100.0),
        ("850-700 hPa", 700.0, 850.0),
        ("700-500 hPa", 500.0, 700.0),
        ("500-300 hPa", 300.0, 500.0),
    ] {
        match layer_mean_rh(&pressure, &temperature, &dewpoint, top, bottom) {
            Some(rh) => out.push_str(&format!("  mean RH {label}: {rh:.0}%\n")),
            None => out.push_str(&format!("  mean RH {label}: --\n")),
        }
    }
    match hodograph_turning(&pressure, &height, &u, &v) {
        Some(turn) if turn.abs() < 10.0 => {
            out.push_str(&format!("  hodograph 0-3 km: straight ({turn:.0} deg)\n"))
        }
        Some(turn) if turn > 0.0 => out.push_str(&format!(
            "  hodograph 0-3 km: clockwise/veering (+{turn:.0} deg)\n"
        )),
        Some(turn) => out.push_str(&format!(
            "  hodograph 0-3 km: counter-clockwise/backing ({turn:.0} deg)\n"
        )),
        None => out.push_str("  hodograph 0-3 km: --\n"),
    }
    out.push_str(&format!("  stored levels: {}\n", pressure.len()));
    if let Some(note) = data.get("ecape_note").and_then(Value::as_str) {
        out.push_str(&format!("  note: {note}\n"));
    }
    out
}

/// The instruction half of the request. Deliberately narrow: describe, do not
/// warn, and never introduce a number that is not in the briefing.
pub const SYSTEM_PROMPT: &str = "\
You write short sounding discussions for operational meteorologists. You are given \
a briefing of MEASURED values. Rules, in order of importance: use only numbers that \
appear in the briefing and never compute or estimate new ones; if a value is `--` it \
is unavailable, so say nothing about it rather than guessing; do not issue warnings, \
advice, or calls to action; do not mention that you are an AI or that this is \
generated. Describe what the profile shows — stability, moisture distribution, wind \
structure — in plain forecaster language. Three sentences maximum.";

/// The request body for an OpenAI-compatible chat completion.
pub fn chat_request(model: &str, briefing: &str, max_sentences_hint: u32) -> Value {
    serde_json::json!({
        "model": model,
        "temperature": 0.2,
        "max_tokens": 60 * max_sentences_hint.max(1),
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": briefing },
        ],
    })
}

/// Where the provider key is read from, in order. Absent means the live call
/// stays disabled and `/api/analysis` serves the briefing only.
pub const KEY_FILE: &str = "/opt/rusty-weather/secrets/openrouter.key";
pub const KEY_ENV: &str = "OPENROUTER_API_KEY";

/// True when a key is configured. The key itself is never returned, logged, or
/// echoed into a response.
pub fn key_configured() -> bool {
    if std::env::var(KEY_ENV).is_ok_and(|value| !value.trim().is_empty()) {
        return true;
    }
    std::fs::read_to_string(KEY_FILE).is_ok_and(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        serde_json::json!({
            "model": "hrrr",
            "run": "20260727_04z",
            "hour": 6,
            "valid": "Mon 7/27 10Z",
            "nearest_place": "Boise City, ID",
            "ecape_note": "entrainment-adjusted (experimental)",
            "indices": {
                "sbcape_j_kg": 0.0,
                "mucape_j_kg": 250.5,
                "lfc_m_agl": null,
                "lapse_700_500_c_km": 7.3,
                "pwat_in": 0.6
            },
            "profile": {
                // Surface-first, pressure decreasing. 900-880 is an inversion.
                "pressure_hpa": [1000.0, 950.0, 900.0, 880.0, 850.0, 700.0, 500.0, 300.0, 200.0],
                "height_m_msl": [100.0, 600.0, 1000.0, 1200.0, 1500.0, 3000.0, 5800.0, 9200.0, 11800.0],
                "temperature_c": [30.0, 25.0, 20.0, 23.0, 18.0, 8.0, -10.0, -40.0, -55.0],
                "dewpoint_c": [15.0, 12.0, 8.0, 2.0, 0.0, -12.0, -30.0, -55.0, -70.0],
                "u_ms": [0.0, 2.0, 4.0, 5.0, 6.0, 10.0, 20.0, 30.0, 35.0],
                "v_ms": [5.0, 5.0, 4.0, 3.0, 2.0, 0.0, -5.0, -10.0, -12.0]
            }
        })
    }

    #[test]
    fn a_missing_value_is_a_dash_not_a_zero() {
        let briefing = briefing_from_sounding(&sample());
        // LFC is null in the payload: an unavailable LFC and a 0 m LFC are
        // different claims, and the model must not read one as the other.
        assert!(briefing.contains("LFC m AGL: --"), "{briefing}");
        // A real zero stays a zero.
        assert!(briefing.contains("SBCAPE J/kg: 0"), "{briefing}");
        // An index the payload omits entirely is also a dash, not absent.
        assert!(briefing.contains("SRH 0-1 km m2/s2: --"), "{briefing}");
    }

    #[test]
    fn structure_is_measured_here_so_the_model_never_has_to() {
        let briefing = briefing_from_sounding(&sample());
        assert!(
            briefing.contains("lowest inversion: 900-880 hPa, +3.0 C"),
            "{briefing}"
        );
        // Veering low-level wind: bearing turns clockwise with height.
        assert!(
            briefing.contains("clockwise/veering"),
            "{briefing}"
        );
        // Layer moisture is stated, so it cannot be eyeballed off a trace.
        assert!(briefing.contains("mean RH surface-850 hPa:"), "{briefing}");
        assert!(briefing.contains("mean RH 700-500 hPa:"), "{briefing}");
    }

    #[test]
    fn mandatory_levels_say_not_stored_rather_than_interpolating() {
        let mut thin = sample();
        thin["profile"]["pressure_hpa"] = serde_json::json!([1000.0, 950.0]);
        thin["profile"]["height_m_msl"] = serde_json::json!([100.0, 600.0]);
        thin["profile"]["temperature_c"] = serde_json::json!([30.0, 25.0]);
        thin["profile"]["dewpoint_c"] = serde_json::json!([15.0, 12.0]);
        thin["profile"]["u_ms"] = serde_json::json!([0.0, 2.0]);
        thin["profile"]["v_ms"] = serde_json::json!([5.0, 5.0]);
        let briefing = briefing_from_sounding(&thin);
        assert!(briefing.contains("500 hPa: not stored"), "{briefing}");
    }

    /// The cache key is the briefing, so the same inputs must give byte-identical
    /// text — no map iteration order, no timestamps.
    #[test]
    fn the_briefing_is_deterministic() {
        let first = briefing_from_sounding(&sample());
        let second = briefing_from_sounding(&sample());
        assert_eq!(first, second);
        // And the index order is the declared one, not JSON order.
        let sbcape = first.find("SBCAPE").expect("sbcape");
        let pwat = first.find("PWAT").expect("pwat");
        assert!(sbcape < pwat, "index order should follow INDEX_FIELDS");
    }

    #[test]
    fn the_prompt_forbids_inventing_numbers() {
        assert!(SYSTEM_PROMPT.contains("never compute or estimate new ones"));
        assert!(SYSTEM_PROMPT.contains("do not issue warnings"));
        let body = chat_request(DEFAULT_MODEL, "briefing", 3);
        assert_eq!(body["model"], DEFAULT_MODEL);
        assert_eq!(body["messages"][1]["content"], "briefing");
        assert!(body["max_tokens"].as_u64().unwrap() <= 240);
    }
}
