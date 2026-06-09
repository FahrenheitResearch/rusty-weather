//! Grid coordinate generation for GRIB1 Grid Description Sections.
//!
//! Supports lat/lon (equidistant cylindrical), Gaussian lat/lon,
//! and Lambert conformal conic projections.

use crate::grib1::parser::{GridDescriptionSection, GridType};
use crate::GribError;

/// A latitude/longitude coordinate pair in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatLon {
    pub lat: f64,
    pub lon: f64,
}

/// Scanning mode flags from GDS byte 28.
///
/// Bit layout (numbered from MSB):
/// - Bit 1: 0 = points scan in +i (east), 1 = points scan in -i (west)
/// - Bit 2: 0 = points scan in -j (south), 1 = points scan in +j (north)
/// - Bit 3: 0 = adjacent points in i-direction are consecutive,
///          1 = adjacent points in j-direction are consecutive
#[derive(Debug, Clone, Copy)]
pub struct ScanningMode {
    /// True if points scan from east to west (negative i direction).
    pub i_negative: bool,
    /// True if points scan from south to north (positive j direction).
    pub j_positive: bool,
    /// True if adjacent points in j-direction are consecutive.
    pub j_consecutive: bool,
}

impl ScanningMode {
    /// Parse scanning mode from the raw byte.
    pub fn from_byte(byte: u8) -> Self {
        ScanningMode {
            i_negative: byte & 0x80 != 0,
            j_positive: byte & 0x40 != 0,
            j_consecutive: byte & 0x20 != 0,
        }
    }
}

/// Generate grid coordinates for a GRIB1 Grid Description Section.
///
/// Returns a vector of `LatLon` coordinates, one per grid point, in the
/// scanning order specified by the GDS. The length equals `ni * nj` for
/// regular grids.
///
/// # Supported grid types
/// - `LatLon` (type 0): Regular latitude/longitude grid
/// - `Gaussian` (type 4): Gaussian latitude/longitude grid (approximated
///   with regular spacing for coordinate generation; true Gaussian latitudes
///   require computing roots of Legendre polynomials)
/// - `LambertConformal` (type 3): Lambert conformal conic projection
///
/// # Errors
/// Returns `GribError::Unpack` for unsupported grid types.
pub fn grid_coordinates(gds: &GridDescriptionSection) -> Result<Vec<LatLon>, GribError> {
    match &gds.grid_type {
        GridType::LatLon {
            ni,
            nj,
            la1,
            lo1,
            la2,
            lo2,
            di,
            dj,
            scanning_mode,
        } => generate_latlon(
            *ni as usize,
            *nj as usize,
            *la1,
            *lo1,
            *la2,
            *lo2,
            *di,
            *dj,
            *scanning_mode,
        ),
        GridType::Gaussian {
            ni,
            nj,
            la1,
            lo1,
            la2,
            lo2,
            di,
            n,
            scanning_mode,
        } => generate_gaussian(
            *ni as usize,
            *nj as usize,
            *la1,
            *lo1,
            *la2,
            *lo2,
            *di,
            *n,
            *scanning_mode,
        ),
        GridType::LambertConformal {
            nx,
            ny,
            la1,
            lo1,
            lov,
            dx,
            dy,
            latin1,
            latin2,
            scanning_mode,
            ..
        } => generate_lambert(
            *nx as usize,
            *ny as usize,
            *la1,
            *lo1,
            *lov,
            *dx,
            *dy,
            *latin1,
            *latin2,
            *scanning_mode,
        ),
        GridType::PolarStereographic {
            nx,
            ny,
            la1,
            lo1,
            lov,
            dx,
            dy,
            scanning_mode,
            projection_center,
            ..
        } => generate_polar_stereographic(
            *nx as usize,
            *ny as usize,
            *la1,
            *lo1,
            *lov,
            *dx,
            *dy,
            *scanning_mode,
            *projection_center,
        ),
        GridType::Unknown(code) => Err(GribError::Unpack(format!(
            "Grid coordinate generation not supported for data representation type {}",
            code
        ))),
    }
}

/// Generate coordinates for a regular lat/lon grid (GDS type 0).
fn generate_latlon(
    ni: usize,
    nj: usize,
    la1: f64,
    lo1: f64,
    la2: f64,
    lo2: f64,
    di: f64,
    dj: f64,
    scanning_mode: u8,
) -> Result<Vec<LatLon>, GribError> {
    let scan = ScanningMode::from_byte(scanning_mode);
    let n_points = ni * nj;
    let mut coords = Vec::with_capacity(n_points);

    // Determine increments based on first/last points when di/dj are not provided
    // (value of 0 means "not given").
    let lon_inc = if di > 0.0 {
        if scan.i_negative {
            -di
        } else {
            di
        }
    } else if ni > 1 {
        (lo2 - lo1) / (ni as f64 - 1.0)
    } else {
        0.0
    };

    let lat_inc = if dj > 0.0 {
        if scan.j_positive {
            dj
        } else {
            -dj
        }
    } else if nj > 1 {
        (la2 - la1) / (nj as f64 - 1.0)
    } else {
        0.0
    };

    if scan.j_consecutive {
        // Adjacent points in j-direction are consecutive
        for i in 0..ni {
            for j in 0..nj {
                let lon = lo1 + (i as f64) * lon_inc;
                let lat = la1 + (j as f64) * lat_inc;
                coords.push(LatLon { lat, lon });
            }
        }
    } else {
        // Adjacent points in i-direction are consecutive (most common)
        for j in 0..nj {
            for i in 0..ni {
                let lat = la1 + (j as f64) * lat_inc;
                let lon = lo1 + (i as f64) * lon_inc;
                coords.push(LatLon { lat, lon });
            }
        }
    }

    Ok(coords)
}

/// Generate coordinates for a Gaussian lat/lon grid (GDS type 4).
///
/// For full accuracy, Gaussian latitudes should be computed as roots of
/// Legendre polynomials. Here we use an iterative Newton-Raphson approach
/// to compute the N Gaussian latitudes between the pole and the equator,
/// then mirror them. This gives exact Gaussian latitudes.
fn generate_gaussian(
    ni: usize,
    nj: usize,
    la1: f64,
    lo1: f64,
    _la2: f64,
    _lo2: f64,
    di: f64,
    n: u16,
    scanning_mode: u8,
) -> Result<Vec<LatLon>, GribError> {
    let scan = ScanningMode::from_byte(scanning_mode);

    // Compute Gaussian latitudes
    let gauss_lats = compute_gaussian_latitudes(n as usize);

    // The grid has 2*N latitudes total. Find the subset that matches nj rows
    // starting from la1.
    let all_lats: Vec<f64> = if scan.j_positive {
        // South to north
        let mut lats: Vec<f64> = gauss_lats.iter().copied().rev().collect();
        lats.extend(gauss_lats.iter().copied());
        lats
    } else {
        // North to south
        let mut lats: Vec<f64> = gauss_lats.iter().copied().collect();
        let southern: Vec<f64> = gauss_lats.iter().rev().map(|l| -l).collect();
        lats.extend(southern);
        lats
    };

    // Find the starting latitude index closest to la1
    let start_idx = all_lats
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            let da = ((**a) - la1).abs();
            let db = ((**b) - la1).abs();
            da.partial_cmp(&db).unwrap()
        })
        .map(|(i, _)| i)
        .unwrap_or(0);

    // Select nj latitudes
    let selected_lats: Vec<f64> = (0..nj)
        .map(|j| {
            let idx = start_idx + j;
            if idx < all_lats.len() {
                all_lats[idx]
            } else {
                // Fallback: linearly extrapolate
                la1 + (j as f64)
                    * if scan.j_positive { 1.0 } else { -1.0 }
                    * (180.0 / (2.0 * n as f64))
            }
        })
        .collect();

    // Longitude increment
    let lon_inc = if di > 0.0 {
        if scan.i_negative {
            -di
        } else {
            di
        }
    } else if ni > 1 {
        360.0 / ni as f64
    } else {
        0.0
    };

    let n_points = ni * nj;
    let mut coords = Vec::with_capacity(n_points);

    if scan.j_consecutive {
        for i in 0..ni {
            for j in 0..nj {
                let lon = lo1 + (i as f64) * lon_inc;
                let lat = selected_lats[j];
                coords.push(LatLon { lat, lon });
            }
        }
    } else {
        for j in 0..nj {
            for i in 0..ni {
                let lat = selected_lats[j];
                let lon = lo1 + (i as f64) * lon_inc;
                coords.push(LatLon { lat, lon });
            }
        }
    }

    Ok(coords)
}

/// Compute N Gaussian latitudes (in degrees) for the Northern Hemisphere,
/// ordered from the pole toward the equator.
///
/// Uses Newton-Raphson iteration to find roots of the Legendre polynomial P_n.
fn compute_gaussian_latitudes(n: usize) -> Vec<f64> {
    let nn = 2 * n;
    let mut lats = Vec::with_capacity(n);

    for i in 0..n {
        // Initial guess using the Bretherton-Hoskins approximation
        let theta = std::f64::consts::PI * (4.0 * (i as f64) + 3.0) / (4.0 * nn as f64 + 2.0);
        let mut x = theta.cos();

        // Newton-Raphson iteration
        for _ in 0..100 {
            let (pn, dpn) = legendre_pn(nn, x);
            let dx = pn / dpn;
            x -= dx;
            if dx.abs() < 1e-15 {
                break;
            }
        }

        lats.push(x.acos().to_degrees().copysign(1.0) * if i < n { 1.0 } else { -1.0 });
        // Convert from colatitude to latitude
        lats[i] = 90.0 - x.acos().to_degrees();
    }

    lats
}

/// Evaluate Legendre polynomial P_n(x) and its derivative P_n'(x)
/// using the three-term recurrence.
fn legendre_pn(n: usize, x: f64) -> (f64, f64) {
    let mut p0 = 1.0;
    let mut p1 = x;

    for k in 2..=n {
        let kf = k as f64;
        let p2 = ((2.0 * kf - 1.0) * x * p1 - (kf - 1.0) * p0) / kf;
        p0 = p1;
        p1 = p2;
    }

    // Derivative: P_n'(x) = n * (x * P_n(x) - P_{n-1}(x)) / (x^2 - 1)
    let dp = (n as f64) * (x * p1 - p0) / (x * x - 1.0);

    (p1, dp)
}

/// Generate coordinates for a Lambert conformal conic grid (GDS type 3).
///
/// Uses the standard Lambert conformal conic projection equations to convert
/// grid (i, j) indices to geographic (lat, lon) coordinates.
fn generate_lambert(
    nx: usize,
    ny: usize,
    la1: f64,
    lo1: f64,
    lov: f64,
    dx: f64,
    dy: f64,
    latin1: f64,
    latin2: f64,
    scanning_mode: u8,
) -> Result<Vec<LatLon>, GribError> {
    let scan = ScanningMode::from_byte(scanning_mode);
    let n_points = nx * ny;
    let mut coords = Vec::with_capacity(n_points);

    let deg_to_rad = std::f64::consts::PI / 180.0;

    // Earth radius in meters (WMO standard for GRIB1)
    let earth_radius = 6_367_470.0;

    let phi1 = latin1 * deg_to_rad;
    let phi2 = latin2 * deg_to_rad;

    // Compute cone constant n
    let n = if (latin1 - latin2).abs() < 1e-6 {
        phi1.sin()
    } else {
        let ln_ratio = (phi2.cos().ln() - phi1.cos().ln())
            / ((std::f64::consts::FRAC_PI_4 + phi2 / 2.0).tan().ln()
                - (std::f64::consts::FRAC_PI_4 + phi1 / 2.0).tan().ln());
        ln_ratio
    };

    let n_val = n;
    let f_val = phi1.cos() * (std::f64::consts::FRAC_PI_4 + phi1 / 2.0).tan().powf(n_val) / n_val;

    // Rho function: rho(phi) = earth_radius * F / tan(pi/4 + phi/2)^n
    let rho = |phi: f64| -> f64 {
        earth_radius * f_val / (std::f64::consts::FRAC_PI_4 + phi / 2.0).tan().powf(n_val)
    };

    // Compute rho0 at the first grid point to establish the coordinate system
    let phi_1 = la1 * deg_to_rad;
    let rho_1 = rho(phi_1);
    let theta_1 = n_val * (lo1 - lov) * deg_to_rad;

    // Map coordinates of the first grid point
    let x0 = rho_1 * theta_1.sin();
    let y0 = rho(90.0_f64 * deg_to_rad) - rho_1 * theta_1.cos();

    // Grid spacing with sign adjustments for scanning mode
    let dx_signed = if scan.i_negative { -dx } else { dx };
    let dy_signed = if scan.j_positive { dy } else { -dy };

    let rho_0 = rho(90.0_f64 * deg_to_rad); // rho at north pole

    if scan.j_consecutive {
        for i in 0..nx {
            for j in 0..ny {
                let x = x0 + (i as f64) * dx_signed;
                let y = y0 + (j as f64) * dy_signed;

                let (lat, lon) = lambert_inverse(x, y, rho_0, n_val, f_val, lov, earth_radius);
                coords.push(LatLon { lat, lon });
            }
        }
    } else {
        for j in 0..ny {
            for i in 0..nx {
                let x = x0 + (i as f64) * dx_signed;
                let y = y0 + (j as f64) * dy_signed;

                let (lat, lon) = lambert_inverse(x, y, rho_0, n_val, f_val, lov, earth_radius);
                coords.push(LatLon { lat, lon });
            }
        }
    }

    Ok(coords)
}

/// Inverse Lambert conformal projection: convert (x, y) map coords to (lat, lon).
fn lambert_inverse(
    x: f64,
    y: f64,
    rho_0: f64,
    n: f64,
    f: f64,
    lov: f64,
    earth_radius: f64,
) -> (f64, f64) {
    let rad_to_deg = 180.0 / std::f64::consts::PI;

    let y_prime = rho_0 - y;
    let rho = (x * x + y_prime * y_prime).sqrt().copysign(n);

    let theta = (x / y_prime).atan();

    let lat = if rho.abs() < 1e-10 {
        90.0_f64.copysign(n)
    } else {
        let t = (earth_radius * f / rho).powf(1.0 / n);
        (2.0 * t.atan() - std::f64::consts::FRAC_PI_2) * rad_to_deg
    };

    let lon = lov + theta * rad_to_deg / n;

    // Normalize longitude to [-180, 180]
    let lon = ((lon + 180.0) % 360.0 + 360.0) % 360.0 - 180.0;

    (lat, lon)
}

/// Generate coordinates for a polar stereographic grid (GDS type 5).
fn generate_polar_stereographic(
    nx: usize,
    ny: usize,
    la1: f64,
    lo1: f64,
    lov: f64,
    dx: f64,
    dy: f64,
    scanning_mode: u8,
    projection_center: u8,
) -> Result<Vec<LatLon>, GribError> {
    let scan = ScanningMode::from_byte(scanning_mode);
    let n_points = nx * ny;
    let mut coords = Vec::with_capacity(n_points);

    let deg_to_rad = std::f64::consts::PI / 180.0;
    let rad_to_deg = 180.0 / std::f64::consts::PI;

    // Earth radius (WMO standard for GRIB1)
    let earth_radius = 6_367_470.0;

    // Standard latitude is 60 degrees for GRIB1 polar stereographic
    let std_lat = 60.0;

    // North or south pole projection
    let north = projection_center & 0x80 == 0;
    let sign = if north { 1.0 } else { -1.0 };

    // Scale factor at standard latitude
    let scale_at_std = (1.0 + (std_lat * deg_to_rad).sin()) / 2.0;

    // Compute (x, y) of first grid point
    let phi1 = la1 * deg_to_rad;
    let lam1 = (lo1 - lov) * deg_to_rad;

    let r1 =
        earth_radius * scale_at_std * ((std::f64::consts::FRAC_PI_4 - sign * phi1 / 2.0).tan());

    let x0 = r1 * (sign * lam1).sin();
    let y0 = -sign * r1 * (sign * lam1).cos();

    let dx_signed = if scan.i_negative { -dx } else { dx };
    let dy_signed = if scan.j_positive { dy } else { -dy };

    if scan.j_consecutive {
        for i in 0..nx {
            for j in 0..ny {
                let x = x0 + (i as f64) * dx_signed;
                let y = y0 + (j as f64) * dy_signed;

                let rho = (x * x + y * y).sqrt();
                let lat = sign
                    * (std::f64::consts::FRAC_PI_2
                        - 2.0 * (rho / (2.0 * earth_radius * scale_at_std)).atan())
                    * rad_to_deg;
                let lon = lov + (x).atan2(-sign * y) * rad_to_deg;
                let lon = ((lon + 180.0) % 360.0 + 360.0) % 360.0 - 180.0;

                coords.push(LatLon { lat, lon });
            }
        }
    } else {
        for j in 0..ny {
            for i in 0..nx {
                let x = x0 + (i as f64) * dx_signed;
                let y = y0 + (j as f64) * dy_signed;

                let rho = (x * x + y * y).sqrt();
                let lat = sign
                    * (std::f64::consts::FRAC_PI_2
                        - 2.0 * (rho / (2.0 * earth_radius * scale_at_std)).atan())
                    * rad_to_deg;
                let lon = lov + (x).atan2(-sign * y) * rad_to_deg;
                let lon = ((lon + 180.0) % 360.0 + 360.0) % 360.0 - 180.0;

                coords.push(LatLon { lat, lon });
            }
        }
    }

    Ok(coords)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scanning_mode_default() {
        // Most common: east, north-to-south, i-consecutive
        let scan = ScanningMode::from_byte(0x00);
        assert!(!scan.i_negative);
        assert!(!scan.j_positive);
        assert!(!scan.j_consecutive);
    }

    #[test]
    fn test_scanning_mode_j_positive() {
        let scan = ScanningMode::from_byte(0x40);
        assert!(!scan.i_negative);
        assert!(scan.j_positive);
        assert!(!scan.j_consecutive);
    }

    #[test]
    fn test_latlon_grid_simple() {
        let gds = GridDescriptionSection {
            section_length: 32,
            nv: 0,
            pv_location: 255,
            data_representation_type: 0,
            grid_type: GridType::LatLon {
                ni: 3,
                nj: 2,
                la1: 90.0,
                lo1: 0.0,
                la2: 45.0,
                lo2: 90.0,
                di: 45.0,
                dj: 45.0,
                scanning_mode: 0x00, // east, north-to-south, i-consecutive
            },
            raw: vec![],
        };

        let coords = grid_coordinates(&gds).unwrap();
        assert_eq!(coords.len(), 6);

        // First row (j=0): lat=90, lon=0,45,90
        assert!((coords[0].lat - 90.0).abs() < 1e-6);
        assert!((coords[0].lon - 0.0).abs() < 1e-6);
        assert!((coords[1].lon - 45.0).abs() < 1e-6);
        assert!((coords[2].lon - 90.0).abs() < 1e-6);

        // Second row (j=1): lat=45 (90 - 45 = 45), lon=0,45,90
        assert!((coords[3].lat - 45.0).abs() < 1e-6);
        assert!((coords[3].lon - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_latlon_grid_south_to_north() {
        let gds = GridDescriptionSection {
            section_length: 32,
            nv: 0,
            pv_location: 255,
            data_representation_type: 0,
            grid_type: GridType::LatLon {
                ni: 2,
                nj: 3,
                la1: -90.0,
                lo1: 0.0,
                la2: 90.0,
                lo2: 180.0,
                di: 180.0,
                dj: 90.0,
                scanning_mode: 0x40, // east, south-to-north
            },
            raw: vec![],
        };

        let coords = grid_coordinates(&gds).unwrap();
        assert_eq!(coords.len(), 6);

        // First row: lat=-90 (starting point, scanning north)
        assert!((coords[0].lat - (-90.0)).abs() < 1e-6);
        // Second row: lat=0
        assert!((coords[2].lat - 0.0).abs() < 1e-6);
        // Third row: lat=90
        assert!((coords[4].lat - 90.0).abs() < 1e-6);
    }
}
