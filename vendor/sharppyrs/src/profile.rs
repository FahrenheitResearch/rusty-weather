//! Analysis view-model for the Skew-T widget, backed by
//! [`sharprs`](https://github.com/FahrenheitResearch/sharprs) — the pure-Rust
//! SHARPpy engine. All numerics live in `sharprs`; this struct just computes
//! and caches, once, everything the widget draws.

use sharprs::params::cape::{
    self, ParcelResult, ParcelType as SharprsParcelType, define_parcel, parcelx,
};
use sharprs::params::indices;
use sharprs::profile::StationInfo;
use sharprs::winds;

use crate::extras;
use crate::utils::qc;

/// Which lifted parcel to display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ParcelType {
    /// Observed surface parcel.
    Surface,
    /// Forecast surface parcel.
    Forecast,
    /// Most unstable parcel in the lowest 300 hPa (the SHARPpy default the
    /// original renderer displays).
    #[default]
    MostUnstable,
    /// 100-hPa mean mixed layer parcel.
    MixedLayer,
}

/// Raw sounding input, ordered surface upward. Missing values may be encoded
/// as `missing` (default -9999), NaN, or anything non-finite. Prefer building
/// a [`sharprs::Profile`] yourself (e.g. via `rustwx-sounding`) and using
/// [`Profile::from_sharprs`]; this exists for convenience.
#[derive(Clone, Debug, Default)]
pub struct SoundingData {
    /// Pressure (hPa).
    pub pres: Vec<f64>,
    /// Height (m MSL).
    pub hght: Vec<f64>,
    /// Temperature (C).
    pub tmpc: Vec<f64>,
    /// Dewpoint (C).
    pub dwpc: Vec<f64>,
    /// Wind direction (degrees from north).
    pub wdir: Vec<f64>,
    /// Wind speed (kts).
    pub wspd: Vec<f64>,
    /// Pressure vertical velocity omega (Pa/s), optional.
    pub omeg: Option<Vec<f64>>,
    /// Latitude (degrees); used for the hemisphere of wind barbs.
    pub latitude: Option<f64>,
    /// Longitude (degrees east); used by the location map panel.
    pub longitude: Option<f64>,
    /// Missing-data sentinel (default -9999.0).
    pub missing: Option<f64>,
}

/// A fully analyzed sounding ready to hand to the [`crate::SkewT`] widget:
/// the `sharprs` profile plus the parcels, effective inflow layer, storm
/// motion, ESRH, max lapse rate, and downdraft trace the display needs.
#[derive(Clone, Debug)]
pub struct Profile {
    /// The underlying `sharprs` profile (raw + derived arrays).
    pub inner: sharprs::Profile,

    /// Surface-based parcel.
    pub sfcpcl: ParcelResult,
    /// Forecast surface parcel.
    pub fcstpcl: ParcelResult,
    /// Most unstable parcel (lowest 300 hPa).
    pub mupcl: ParcelResult,
    /// 100-hPa mixed layer parcel.
    pub mlpcl: ParcelResult,

    /// Effective inflow layer bottom/top pressure (hPa; NaN when absent).
    pub ebottom: f64,
    pub etop: f64,
    /// Effective inflow layer bottom/top height (m AGL; NaN when absent).
    pub ebotm: f64,
    pub etopm: f64,
    /// Bunkers storm motion `(right_u, right_v, left_u, left_v)` (kts),
    /// parcel-based like the original renderer.
    pub srwind: (f64, f64, f64, f64),
    /// Effective SRH (right mover) (m2/s2).
    pub right_esrh: f64,
    /// Max 2-6 km AGL lapse rate: `(value C/km, pbot hPa, ptop hPa)`.
    pub max_lapse_rate_2_6: (f64, f64, f64),
    /// DCAPE (J/kg).
    pub dcape: f64,
    /// Downdraft parcel trace (C / hPa).
    pub dpcl_ttrace: Vec<f64>,
    pub dpcl_ptrace: Vec<f64>,
}

fn clean(v: &[f64], missing: f64) -> Vec<f64> {
    v.iter()
        .map(|x| {
            if !x.is_finite() || (*x - missing).abs() < 1e-6 || *x <= -9990.0 {
                f64::NAN
            } else {
                *x
            }
        })
        .collect()
}

impl Profile {
    /// Analyze a raw sounding. Returns `None` when the input is unusable.
    pub fn new(data: SoundingData) -> Option<Profile> {
        let missing = data.missing.unwrap_or(-9999.0);
        let omeg = data.omeg.map(|o| clean(&o, missing)).unwrap_or_default();
        let station = StationInfo {
            station_id: String::new(),
            latitude: data.latitude.unwrap_or(35.0),
            longitude: data.longitude.unwrap_or(f64::NAN),
            elevation: f64::NAN,
            datetime: String::new(),
        };
        let inner = sharprs::Profile::new(
            &clean(&data.pres, missing),
            &clean(&data.hght, missing),
            &clean(&data.tmpc, missing),
            &clean(&data.dwpc, missing),
            &clean(&data.wdir, missing),
            &clean(&data.wspd, missing),
            &omeg,
            station,
        )
        .ok()?;
        Some(Profile::from_sharprs(inner))
    }

    /// Analyze an existing [`sharprs::Profile`] (e.g. one produced by
    /// `rustwx-sounding`).
    pub fn from_sharprs(inner: sharprs::Profile) -> Profile {
        // The parcel routines take sharprs's lean cape::Profile (same bridge
        // sharprs's own compositor uses).
        let cape_prof = cape::Profile::new(
            inner.pres.clone(),
            inner.hght.clone(),
            inner.tmpc.clone(),
            inner.dwpc.clone(),
            inner.sfc,
        );
        let lift = |ptype: SharprsParcelType| {
            parcelx(&cape_prof, &define_parcel(&cape_prof, ptype), None, None)
        };
        let mupcl = lift(SharprsParcelType::MostUnstable { depth_hpa: 300.0 });
        let sfcpcl = if mupcl.pres == inner.pres[inner.sfc] {
            mupcl.clone()
        } else {
            lift(SharprsParcelType::Surface)
        };
        let fcstpcl = extras::forecast_parcel(&inner, &cape_prof);
        let mlpcl = lift(SharprsParcelType::MixedLayer { depth_hpa: 100.0 });

        let (ebottom, etop) = cape::effective_inflow_layer(&cape_prof, 100.0, -250.0, Some(&mupcl));
        let mut ebotm = f64::NAN;
        let mut etopm = f64::NAN;
        let mut right_esrh = f64::NAN;
        let srwind;
        if qc(ebottom) && qc(etop) {
            ebotm = inner.to_agl(inner.interp_hght(ebottom));
            etopm = inner.to_agl(inner.interp_hght(etop));
            srwind = extras::bunkers_storm_motion(&inner, &mupcl, ebottom);
            if qc(ebotm) && qc(etopm) {
                right_esrh = extras::helicity(&inner, ebotm, etopm, srwind.0, srwind.1).0;
            }
        } else {
            srwind = winds::non_parcel_bunkers_motion(&inner).unwrap_or((
                f64::NAN,
                f64::NAN,
                f64::NAN,
                f64::NAN,
            ));
        }

        let mlr = indices::max_lapse_rate(
            &inner,
            Some(2000.0),
            Some(6000.0),
            Some(250.0),
            Some(2000.0),
        );
        let max_lapse_rate_2_6 =
            mlr.map(|m| (m.value, m.pbot, m.ptop))
                .unwrap_or((f64::NAN, f64::NAN, f64::NAN));

        let dc = cape::dcape(&cape_prof);

        Profile {
            inner,
            sfcpcl,
            fcstpcl,
            mupcl,
            mlpcl,
            ebottom,
            etop,
            ebotm,
            etopm,
            srwind,
            right_esrh,
            max_lapse_rate_2_6,
            dcape: dc.dcape,
            dpcl_ttrace: dc.ttrace,
            dpcl_ptrace: dc.ptrace,
        }
    }

    /// The parcel of the given type (already computed).
    pub fn parcel(&self, kind: ParcelType) -> &ParcelResult {
        match kind {
            ParcelType::Surface => &self.sfcpcl,
            ParcelType::Forecast => &self.fcstpcl,
            ParcelType::MostUnstable => &self.mupcl,
            ParcelType::MixedLayer => &self.mlpcl,
        }
    }

    /// Latitude (degrees north) from the station metadata.
    pub fn latitude(&self) -> f64 {
        self.inner.station.latitude
    }
}
