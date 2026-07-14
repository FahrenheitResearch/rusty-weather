//! Compute ecape-parcel-compatible analytic MU ECAPE for one sounding.

use std::io::{self, Read};

use ecape_rs::{CapeType, ParcelOptions, StormMotionType, calc_ecape_ncape};
use serde::{Deserialize, Serialize};

const KTS_TO_MS: f64 = 0.514_444_444_444_444_5;

#[derive(Deserialize)]
struct Input {
    pressure_hpa: Vec<f64>,
    height_m_msl: Vec<f64>,
    temperature_c: Vec<f64>,
    dewpoint_c: Vec<f64>,
    u_knots: Vec<f64>,
    v_knots: Vec<f64>,
}

#[derive(Serialize)]
struct Output {
    schema: &'static str,
    method: &'static str,
    ecape_jkg: f64,
    ncape_jkg: f64,
    cape_jkg: f64,
    lfc_m_msl: Option<f64>,
    el_m_msl: Option<f64>,
}

fn specific_humidity(pressure_pa: f64, dewpoint_c: f64) -> f64 {
    let vapor_pressure = 611.2 * ((17.67 * dewpoint_c) / (dewpoint_c + 243.5)).exp();
    0.62197 * vapor_pressure / (pressure_pa - 0.37803 * vapor_pressure)
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut raw = String::new();
    io::stdin().read_to_string(&mut raw)?;
    let input: Input = serde_json::from_str(&raw)?;
    let pressure_pa: Vec<f64> = input
        .pressure_hpa
        .iter()
        .map(|value| value * 100.0)
        .collect();
    let temperature_k: Vec<f64> = input
        .temperature_c
        .iter()
        .map(|value| value + 273.15)
        .collect();
    let qv: Vec<f64> = pressure_pa
        .iter()
        .zip(&input.dewpoint_c)
        .map(|(pressure, dewpoint)| specific_humidity(*pressure, *dewpoint))
        .collect();
    let u_ms: Vec<f64> = input
        .u_knots
        .iter()
        .map(|value| value * KTS_TO_MS)
        .collect();
    let v_ms: Vec<f64> = input
        .v_knots
        .iter()
        .map(|value| value * KTS_TO_MS)
        .collect();
    let options = ParcelOptions {
        cape_type: CapeType::MostUnstable,
        storm_motion_type: StormMotionType::RightMoving,
        pseudoadiabatic: Some(true),
        ..ParcelOptions::default()
    };
    let result = calc_ecape_ncape(
        &input.height_m_msl,
        &pressure_pa,
        &temperature_k,
        &qv,
        &u_ms,
        &v_ms,
        &options,
    )?;
    serde_json::to_writer(
        io::stdout().lock(),
        &Output {
            schema: "sharpmod.ecape.v1",
            method: "ecape-rs analytic most-unstable pseudoadiabatic",
            ecape_jkg: result.ecape_jkg,
            ncape_jkg: result.ncape_jkg,
            cape_jkg: result.cape_jkg,
            lfc_m_msl: result.lfc_m,
            el_m_msl: result.el_m,
        },
    )?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    run()
}
