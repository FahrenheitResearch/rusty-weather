//! Historical observed-sounding loader.
//!
//! IEM provides a compact station catalog plus archive CSV services for RAOB
//! and ASOS observations. Network and parsing work stays off the egui thread.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use rustwx_sounding::{SoundingColumn, SoundingMetadata};
use serde::Deserialize;

const RAOB_NETWORK_URL: &str = "https://mesonet.agron.iastate.edu/geojson/network.py?network=RAOB";
const RAOB_URL: &str = "https://mesonet.agron.iastate.edu/cgi-bin/request/raob.py";
const ASOS_URL: &str = "https://mesonet.agron.iastate.edu/cgi-bin/request/asos.py";
const KT_TO_MS: f64 = 0.514_444_444_444_444_5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObsKind {
    Raob,
    SurfaceAdjusted,
}

#[derive(Debug, Clone)]
pub struct ObsRequest {
    pub kind: ObsKind,
    pub valid_unix: i64,
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug)]
pub struct LoadedObservation {
    pub kind: ObsKind,
    pub column: SoundingColumn,
    pub heading: String,
    pub subheading: String,
    pub read_ms: f32,
}

pub struct ObsResponse {
    pub kind: ObsKind,
    pub result: Result<LoadedObservation, String>,
}

pub struct ObsWorker {
    tx: Sender<ObsRequest>,
    rx: Receiver<ObsResponse>,
}

impl ObsWorker {
    pub fn spawn(notify: impl Fn() + Send + 'static) -> Self {
        let (request_tx, request_rx) = channel::<ObsRequest>();
        let (response_tx, response_rx) = channel::<ObsResponse>();
        std::thread::Builder::new()
            .name("rw-observed-soundings".into())
            .spawn(move || worker_main(request_rx, response_tx, notify))
            .expect("spawn observed sounding worker");
        Self {
            tx: request_tx,
            rx: response_rx,
        }
    }

    pub fn send(&self, request: ObsRequest) {
        let _ = self.tx.send(request);
    }

    pub fn try_recv(&self) -> Option<ObsResponse> {
        match self.rx.try_recv() {
            Ok(value) => Some(value),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }
}

fn worker_main(requests: Receiver<ObsRequest>, responses: Sender<ObsResponse>, notify: impl Fn()) {
    let agent = build_agent();
    let mut stations: Option<Vec<RaobStation>> = None;
    while let Ok(request) = requests.recv() {
        let kind = request.kind;
        let started = Instant::now();
        let result = load_observation(&agent, &mut stations, request).map(|mut loaded| {
            loaded.read_ms = started.elapsed().as_secs_f32() * 1000.0;
            loaded
        });
        let _ = responses.send(ObsResponse { kind, result });
        notify();
    }
}

fn build_agent() -> ureq::Agent {
    static PROVIDER: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    PROVIDER.get_or_init(|| {
        rustls::crypto::CryptoProvider::install_default(rustls_rustcrypto::provider()).ok();
    });
    let crypto = std::sync::Arc::new(rustls_rustcrypto::provider());
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(45)))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .root_certs(ureq::tls::RootCerts::WebPki)
                .unversioned_rustls_crypto_provider(crypto)
                .build(),
        )
        .build()
        .new_agent()
}

fn get_text(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let mut response = agent.get(url).call().map_err(|error| error.to_string())?;
    response
        .body_mut()
        .read_to_string()
        .map_err(|error| error.to_string())
}

fn load_observation(
    agent: &ureq::Agent,
    station_cache: &mut Option<Vec<RaobStation>>,
    request: ObsRequest,
) -> Result<LoadedObservation, String> {
    if station_cache.is_none() {
        *station_cache = Some(fetch_raob_stations(agent)?);
    }
    let target = Utc
        .timestamp_opt(request.valid_unix, 0)
        .single()
        .ok_or_else(|| "selected valid time is outside the supported UTC range".to_string())?;
    let nearest = nearest_raob(
        agent,
        station_cache.as_deref().unwrap_or_default(),
        target,
        request.latitude,
        request.longitude,
    )?;

    let raob_time = nearest.column.metadata.valid_time.clone();
    let mut column = nearest.column;
    let mut adjustment = None;
    if request.kind == ObsKind::SurfaceAdjusted {
        if let Some(surface) =
            nearest_surface_obs(agent, target, request.latitude, request.longitude)?
        {
            apply_surface_observation(&mut column, &surface);
            adjustment = Some(surface);
        }
    }

    let raob_station = nearest.station.id.clone();
    let heading = match &adjustment {
        Some(surface) => format!("Observed adjusted · {raob_station} + {}", surface.station),
        None => format!("RAOB · {raob_station}"),
    };
    let subheading = match &adjustment {
        Some(surface) => format!(
            "RAOB {raob_time} ({:.0} km from point) · surface {} {} ({:.0} km)",
            nearest.distance_km,
            surface.station,
            format_utc(surface.time),
            surface.distance_km
        ),
        None => format!(
            "{} · {raob_time} · {:.0} km from selected point · {:.1} h from selected time",
            nearest.station.name, nearest.distance_km, nearest.time_delta_hours
        ),
    };

    Ok(LoadedObservation {
        kind: request.kind,
        column,
        heading,
        subheading,
        read_ms: 0.0,
    })
}

#[derive(Debug, Clone)]
struct RaobStation {
    id: String,
    name: String,
    latitude: f64,
    longitude: f64,
    elevation_m: Option<f64>,
}

#[derive(Deserialize)]
struct StationCollection {
    features: Vec<StationFeature>,
}

#[derive(Deserialize)]
struct StationFeature {
    id: String,
    geometry: StationGeometry,
    properties: StationProperties,
}

#[derive(Deserialize)]
struct StationGeometry {
    coordinates: [f64; 2],
}

#[derive(Deserialize)]
struct StationProperties {
    #[serde(default)]
    sname: String,
    elevation: Option<f64>,
}

fn fetch_raob_stations(agent: &ureq::Agent) -> Result<Vec<RaobStation>, String> {
    let text = get_text(agent, RAOB_NETWORK_URL)?;
    let collection: StationCollection = serde_json::from_str(&text)
        .map_err(|error| format!("invalid RAOB station list: {error}"))?;
    let stations = collection
        .features
        .into_iter()
        .filter_map(|feature| {
            let [longitude, latitude] = feature.geometry.coordinates;
            (latitude.is_finite() && longitude.is_finite()).then_some(RaobStation {
                id: feature.id,
                name: feature.properties.sname,
                latitude,
                longitude,
                elevation_m: feature.properties.elevation,
            })
        })
        .collect::<Vec<_>>();
    (!stations.is_empty())
        .then_some(stations)
        .ok_or_else(|| "IEM returned an empty RAOB station list".to_string())
}

struct NearestRaob {
    station: RaobStation,
    column: SoundingColumn,
    distance_km: f64,
    time_delta_hours: f64,
}

fn nearest_raob(
    agent: &ureq::Agent,
    stations: &[RaobStation],
    target: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<NearestRaob, String> {
    let mut candidates = stations
        .iter()
        .map(|station| {
            (
                haversine_km(latitude, longitude, station.latitude, station.longitude),
                station,
            )
        })
        .filter(|(distance, _)| distance.is_finite())
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| a.0.total_cmp(&b.0));

    let start = target - chrono::Duration::hours(7);
    let end = target + chrono::Duration::hours(7);
    let mut best: Option<(f64, NearestRaob)> = None;
    let mut errors = Vec::new();
    for (distance_km, station) in candidates.into_iter().take(6) {
        let url = format!(
            "{RAOB_URL}?station={}&sts={}&ets={}",
            station.id,
            query_utc(start),
            query_utc(end)
        );
        let text = match get_text(agent, &url) {
            Ok(text) => text,
            Err(error) => {
                errors.push(format!("{}: {error}", station.id));
                continue;
            }
        };
        let profiles = match parse_raob_csv(&text, station) {
            Ok(value) => value,
            Err(error) => {
                errors.push(format!("{}: {error}", station.id));
                continue;
            }
        };
        for (time, column) in profiles {
            let delta_hours =
                (time.timestamp() - target.timestamp()).unsigned_abs() as f64 / 3600.0;
            if delta_hours > 7.0 {
                continue;
            }
            // Distance dominates, but do not prefer a six-hour-old launch to
            // an almost-as-close launch near the requested time.
            let score = distance_km + delta_hours * 35.0;
            let candidate = NearestRaob {
                station: station.clone(),
                column,
                distance_km,
                time_delta_hours: delta_hours,
            };
            if best.as_ref().is_none_or(|(old, _)| score < *old) {
                best = Some((score, candidate));
            }
        }
    }
    best.map(|(_, value)| value).ok_or_else(|| {
        let detail = if errors.is_empty() {
            "no launches were returned in the ±7 hour window".to_string()
        } else {
            errors.join("; ")
        };
        format!("No usable nearby RAOB for {}: {detail}", format_utc(target))
    })
}

#[derive(Default)]
struct RaobRows {
    pressure: Vec<f64>,
    height: Vec<f64>,
    temperature: Vec<f64>,
    dewpoint: Vec<f64>,
    u_ms: Vec<f64>,
    v_ms: Vec<f64>,
}

fn parse_raob_csv(
    text: &str,
    station: &RaobStation,
) -> Result<Vec<(DateTime<Utc>, SoundingColumn)>, String> {
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| "empty CSV response".to_string())?;
    let headers = csv_fields(header);
    let index = |name: &str| {
        headers
            .iter()
            .position(|header| header.eq_ignore_ascii_case(name))
            .ok_or_else(|| format!("RAOB CSV is missing {name}"))
    };
    let itime = index("validUTC")?;
    let ip = index("pressure_mb")?;
    let iz = index("height_m")?;
    let it = index("tmpc")?;
    let itd = index("dwpc")?;
    let idir = index("drct")?;
    let ispd = index("speed_kts")?;
    let mut groups: HashMap<i64, RaobRows> = HashMap::new();
    for line in lines {
        let fields = csv_fields(line);
        let Some(time) = fields.get(itime).and_then(|value| parse_iem_time(value)) else {
            continue;
        };
        let values = [ip, iz, it, itd, idir, ispd]
            .map(|i| fields.get(i).and_then(|value| parse_number(value)));
        let [Some(p), Some(z), Some(t), Some(td), Some(dir), Some(speed)] = values else {
            continue;
        };
        if !(p > 0.0 && z.is_finite() && t.is_finite() && td.is_finite()) {
            continue;
        }
        let radians = dir.to_radians();
        let row = groups.entry(time.timestamp()).or_default();
        row.pressure.push(p);
        row.height.push(z);
        row.temperature.push(t);
        row.dewpoint.push(td.min(t));
        row.u_ms.push(-speed * radians.sin() * KT_TO_MS);
        row.v_ms.push(-speed * radians.cos() * KT_TO_MS);
    }

    let mut profiles = Vec::new();
    for (timestamp, rows) in groups {
        if rows.pressure.len() < 8 {
            continue;
        }
        let time = Utc.timestamp_opt(timestamp, 0).single().unwrap();
        let column = SoundingColumn {
            pressure_hpa: rows.pressure,
            height_m_msl: rows.height,
            temperature_c: rows.temperature,
            dewpoint_c: rows.dewpoint,
            u_ms: rows.u_ms,
            v_ms: rows.v_ms,
            omega_pa_s: Vec::new(),
            metadata: SoundingMetadata {
                station_id: station.id.clone(),
                valid_time: format_utc(time),
                latitude_deg: Some(station.latitude),
                longitude_deg: Some(station.longitude),
                elevation_m: station.elevation_m,
                sample_method: Some("IEM archived RAOB".into()),
                ..Default::default()
            },
        };
        if column.validate().is_ok() {
            profiles.push((time, column));
        }
    }
    Ok(profiles)
}

#[derive(Debug)]
struct SurfaceObs {
    station: String,
    time: DateTime<Utc>,
    temperature_c: f64,
    dewpoint_c: f64,
    wind_direction_deg: f64,
    wind_speed_kt: f64,
    distance_km: f64,
}

fn nearest_surface_obs(
    agent: &ureq::Agent,
    target: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<Option<SurfaceObs>, String> {
    let start = target - chrono::Duration::minutes(90);
    let end = target + chrono::Duration::minutes(90);
    let url = format!(
        "{ASOS_URL}?data=tmpf&data=dwpf&data=drct&data=sknt&sts={}&ets={}&tz=Etc%2FUTC&format=onlycomma&latlon=yes&elev=yes&missing=empty",
        query_utc(start),
        query_utc(end)
    );
    let text = get_text(agent, &url)?;
    parse_nearest_surface_csv(&text, target, latitude, longitude)
}

fn parse_nearest_surface_csv(
    text: &str,
    target: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<Option<SurfaceObs>, String> {
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| "empty ASOS CSV response".to_string())?;
    let headers = csv_fields(header);
    let index = |name: &str| {
        headers
            .iter()
            .position(|header| header.eq_ignore_ascii_case(name))
            .ok_or_else(|| format!("ASOS CSV is missing {name}"))
    };
    let ista = index("station")?;
    let itime = index("valid")?;
    let ilon = index("lon")?;
    let ilat = index("lat")?;
    let it = index("tmpf")?;
    let itd = index("dwpf")?;
    let idir = index("drct")?;
    let ispd = index("sknt")?;
    let mut best: Option<(f64, SurfaceObs)> = None;
    for line in lines {
        let fields = csv_fields(line);
        let Some(time) = fields.get(itime).and_then(|value| parse_iem_minute(value)) else {
            continue;
        };
        let Some(station) = fields.get(ista).filter(|value| !value.is_empty()) else {
            continue;
        };
        let values = [ilon, ilat, it, itd, idir, ispd]
            .map(|i| fields.get(i).and_then(|value| parse_number(value)));
        let [
            Some(lon),
            Some(lat),
            Some(tf),
            Some(tdf),
            Some(dir),
            Some(speed),
        ] = values
        else {
            continue;
        };
        let distance_km = haversine_km(latitude, longitude, lat, lon);
        if !distance_km.is_finite() || distance_km > 350.0 {
            continue;
        }
        let time_minutes = (time.timestamp() - target.timestamp()).unsigned_abs() as f64 / 60.0;
        let score = distance_km + time_minutes * 0.75;
        let obs = SurfaceObs {
            station: station.to_string(),
            time,
            temperature_c: (tf - 32.0) * (5.0 / 9.0),
            dewpoint_c: (tdf - 32.0) * (5.0 / 9.0),
            wind_direction_deg: dir,
            wind_speed_kt: speed,
            distance_km,
        };
        if best.as_ref().is_none_or(|(old, _)| score < *old) {
            best = Some((score, obs));
        }
    }
    Ok(best.map(|(_, obs)| obs))
}

fn apply_surface_observation(column: &mut SoundingColumn, surface: &SurfaceObs) {
    if column.pressure_hpa.is_empty() {
        return;
    }
    let radians = surface.wind_direction_deg.to_radians();
    column.temperature_c[0] = surface.temperature_c;
    column.dewpoint_c[0] = surface.dewpoint_c.min(surface.temperature_c);
    column.u_ms[0] = -surface.wind_speed_kt * radians.sin() * KT_TO_MS;
    column.v_ms[0] = -surface.wind_speed_kt * radians.cos() * KT_TO_MS;
    column.metadata.station_id =
        format!("{} + {} SFC", column.metadata.station_id, surface.station);
    column.metadata.valid_time = format!(
        "RAOB {} · SFC {}",
        column.metadata.valid_time,
        format_utc(surface.time)
    );
    column.metadata.sample_method = Some("nearest RAOB adjusted with nearest timed ASOS".into());
}

pub fn load_sounding_file(path: &Path) -> Result<(SoundingColumn, String, String), String> {
    let started = Instant::now();
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let profile = if text.contains("%RAW%") {
        sharprs::Profile::from_sharppy_text(&text)
    } else if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("csv"))
    {
        sharprs::Profile::from_csv(&text)
    } else {
        sharprs::Profile::from_wyoming(&text).or_else(|_| sharprs::Profile::from_csv(&text))
    }
    .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    let mut column = SoundingColumn::from_sharprs_profile(&profile);
    if column.metadata.station_id.trim().is_empty() {
        column.metadata.station_id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("loaded sounding")
            .to_string();
    }
    if column.metadata.valid_time.trim().is_empty() {
        column.metadata.valid_time = "time not specified in file".into();
    }
    column.validate().map_err(|error| error.to_string())?;
    let heading = format!("File · {}", column.metadata.station_id);
    let subheading = format!(
        "{} · {} levels · parsed in {:.0} ms",
        path.display(),
        column.len(),
        started.elapsed().as_secs_f32() * 1000.0
    );
    Ok((column, heading, subheading))
}

fn csv_fields(line: &str) -> Vec<&str> {
    line.trim_end_matches('\r')
        .split(',')
        .map(str::trim)
        .collect()
}

fn parse_number(value: &str) -> Option<f64> {
    if value.is_empty() || value.eq_ignore_ascii_case("M") || value == "-9999" {
        return None;
    }
    value.parse::<f64>().ok().filter(|value| value.is_finite())
}

fn parse_iem_time(value: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|time| Utc.from_utc_datetime(&time))
}

fn parse_iem_minute(value: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M")
        .ok()
        .map(|time| Utc.from_utc_datetime(&time))
}

fn query_utc(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn format_utc(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%d %H:%MZ").to_string()
}

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let earth_radius_km = 6371.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat * 0.5).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon * 0.5).sin().powi(2);
    2.0 * earth_radius_km * a.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iem_raob_csv_into_a_valid_column() {
        let csv = "station,validUTC,levelcode,pressure_mb,height_m,tmpc,dwpc,drct,speed_kts,bearing,range_sm\n\
KOUN,2025-03-15 00:00:00,4,1000,300,20,15,180,10,M,M\n\
KOUN,2025-03-15 00:00:00,4,925,800,15,10,190,15,M,M\n\
KOUN,2025-03-15 00:00:00,4,850,1500,10,5,200,20,M,M\n\
KOUN,2025-03-15 00:00:00,4,700,3000,0,-5,210,25,M,M\n\
KOUN,2025-03-15 00:00:00,4,500,5600,-20,-30,220,30,M,M\n\
KOUN,2025-03-15 00:00:00,4,400,7200,-30,-40,230,35,M,M\n\
KOUN,2025-03-15 00:00:00,4,300,9200,-45,-55,240,40,M,M\n\
KOUN,2025-03-15 00:00:00,4,200,12000,-55,-65,250,45,M,M\n";
        let station = RaobStation {
            id: "KOUN".into(),
            name: "Norman".into(),
            latitude: 35.22,
            longitude: -97.44,
            elevation_m: Some(357.0),
        };
        let profiles = parse_raob_csv(csv, &station).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].1.len(), 8);
        profiles[0].1.validate().unwrap();
    }

    #[test]
    fn parses_common_sounding_file_formats() {
        let text = "%TITLE%\nOUN 250315/0000\n%RAW%\n\
1000,300,20,15,180,10\n925,800,15,10,190,15\n%END%\n";
        let profile = sharprs::Profile::from_sharppy_text(text).unwrap();
        let column = SoundingColumn::from_sharprs_profile(&profile);
        assert_eq!(column.len(), 2);
        assert_eq!(column.metadata.station_id, "OUN");
    }

    #[test]
    #[ignore = "hits the live IEM archive"]
    fn loads_historical_raob_and_surface_adjustment() {
        let agent = build_agent();
        let mut stations = None;
        let request = ObsRequest {
            kind: ObsKind::SurfaceAdjusted,
            valid_unix: 1_741_996_800, // 2025-03-15 00:00Z
            latitude: 35.22,
            longitude: -97.44,
        };
        let loaded = load_observation(&agent, &mut stations, request).unwrap();
        loaded.column.validate().unwrap();
        assert!(loaded.column.metadata.station_id.contains("SFC"));
    }
}
