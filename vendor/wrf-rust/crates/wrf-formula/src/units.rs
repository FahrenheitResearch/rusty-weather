use crate::error::{ErrorKind, FormulaError, FormulaResult};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Integer exponents for the seven SI base dimensions in the order
/// length, mass, time, thermodynamic temperature, current, amount, luminosity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Dimension(pub [i16; 7]);

impl Dimension {
    pub const DIMENSIONLESS: Self = Self([0; 7]);
    pub const LENGTH: Self = Self([1, 0, 0, 0, 0, 0, 0]);
    pub const MASS: Self = Self([0, 1, 0, 0, 0, 0, 0]);
    pub const TIME: Self = Self([0, 0, 1, 0, 0, 0, 0]);
    pub const TEMPERATURE: Self = Self([0, 0, 0, 1, 0, 0, 0]);

    pub fn checked_add(self, rhs: Self) -> FormulaResult<Self> {
        let mut out = [0_i16; 7];
        for (index, slot) in out.iter_mut().enumerate() {
            *slot = self.0[index]
                .checked_add(rhs.0[index])
                .ok_or_else(|| FormulaError::new(ErrorKind::Unit, "unit exponent overflow"))?;
        }
        Ok(Self(out))
    }

    pub fn checked_sub(self, rhs: Self) -> FormulaResult<Self> {
        let mut out = [0_i16; 7];
        for (index, slot) in out.iter_mut().enumerate() {
            *slot = self.0[index]
                .checked_sub(rhs.0[index])
                .ok_or_else(|| FormulaError::new(ErrorKind::Unit, "unit exponent overflow"))?;
        }
        Ok(Self(out))
    }

    pub fn checked_mul(self, power: i16) -> FormulaResult<Self> {
        let mut out = [0_i16; 7];
        for (index, slot) in out.iter_mut().enumerate() {
            *slot = self.0[index]
                .checked_mul(power)
                .ok_or_else(|| FormulaError::new(ErrorKind::Unit, "unit exponent overflow"))?;
        }
        Ok(Self(out))
    }
}

/// Absolute temperatures and temperature differences deliberately have
/// different arithmetic rules, despite sharing an SI dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemperatureKind {
    None,
    Absolute,
    Difference,
}

/// A unit attached to values stored internally in coherent SI units.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unit {
    pub dimension: Dimension,
    pub temperature_kind: TemperatureKind,
    pub symbol: String,
    /// `si = displayed * scale + offset`.
    pub scale: f64,
    pub offset: f64,
    #[serde(default)]
    pub logarithmic: bool,
}

impl Unit {
    pub fn dimensionless() -> Self {
        Self::linear(Dimension::DIMENSIONLESS, "1", 1.0)
    }

    pub fn linear(dimension: Dimension, symbol: impl Into<String>, scale: f64) -> Self {
        Self {
            dimension,
            temperature_kind: TemperatureKind::None,
            symbol: symbol.into(),
            scale,
            offset: 0.0,
            logarithmic: false,
        }
    }

    pub fn absolute_temperature(symbol: impl Into<String>, scale: f64, offset: f64) -> Self {
        Self {
            dimension: Dimension::TEMPERATURE,
            temperature_kind: TemperatureKind::Absolute,
            symbol: symbol.into(),
            scale,
            offset,
            logarithmic: false,
        }
    }

    pub fn temperature_difference(symbol: impl Into<String>, scale: f64) -> Self {
        Self {
            dimension: Dimension::TEMPERATURE,
            temperature_kind: TemperatureKind::Difference,
            symbol: symbol.into(),
            scale,
            offset: 0.0,
            logarithmic: false,
        }
    }

    pub fn is_dimensionless(&self) -> bool {
        self.dimension == Dimension::DIMENSIONLESS && self.temperature_kind == TemperatureKind::None
    }

    pub fn is_absolute_temperature(&self) -> bool {
        self.temperature_kind == TemperatureKind::Absolute
    }

    pub fn compatible(&self, other: &Self) -> bool {
        self.dimension == other.dimension
    }

    pub fn to_si(&self, value: f64) -> f64 {
        value * self.scale + self.offset
    }

    pub fn from_si(&self, value: f64) -> f64 {
        (value - self.offset) / self.scale
    }

    pub(crate) fn canonical(dimension: Dimension, temperature_kind: TemperatureKind) -> Self {
        let temperature_kind =
            if dimension == Dimension::TEMPERATURE && temperature_kind == TemperatureKind::None {
                TemperatureKind::Difference
            } else {
                temperature_kind
            };
        let symbol = canonical_symbol(dimension, temperature_kind);
        Self {
            dimension,
            temperature_kind,
            symbol,
            scale: 1.0,
            offset: 0.0,
            logarithmic: false,
        }
    }

    pub(crate) fn multiply(&self, rhs: &Self) -> FormulaResult<Self> {
        if self.logarithmic || rhs.logarithmic {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                "logarithmic quantities require explicit linear conversion before multiplication",
            ));
        }
        let dimension = self.dimension.checked_add(rhs.dimension)?;
        let kind = if rhs.is_dimensionless() {
            self.temperature_kind
        } else if self.is_dimensionless() {
            rhs.temperature_kind
        } else {
            TemperatureKind::None
        };
        Ok(Self::canonical(dimension, kind))
    }

    pub(crate) fn divide(&self, rhs: &Self) -> FormulaResult<Self> {
        if self.logarithmic || rhs.logarithmic {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                "logarithmic quantities require explicit linear conversion before division",
            ));
        }
        let dimension = self.dimension.checked_sub(rhs.dimension)?;
        let kind = if rhs.is_dimensionless() {
            self.temperature_kind
        } else {
            TemperatureKind::None
        };
        Ok(Self::canonical(dimension, kind))
    }

    pub(crate) fn integer_power(&self, power: i16) -> FormulaResult<Self> {
        if self.logarithmic {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                "logarithmic quantities require explicit linear conversion before exponentiation",
            ));
        }
        Ok(Self::canonical(
            self.dimension.checked_mul(power)?,
            if power == 1 {
                self.temperature_kind
            } else {
                TemperatureKind::None
            },
        ))
    }

    pub(crate) fn square_root(&self) -> FormulaResult<Self> {
        if self.temperature_kind == TemperatureKind::Absolute || self.logarithmic {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                "absolute temperatures cannot be square-rooted",
            ));
        }
        if self.dimension.0.iter().any(|power| power % 2 != 0) {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                format!("sqrt requires even SI exponents, got {}", self),
            ));
        }
        let mut powers = self.dimension.0;
        for power in &mut powers {
            *power /= 2;
        }
        Ok(Self::canonical(Dimension(powers), TemperatureKind::None))
    }

    pub(crate) fn derivative_by(&self, coordinate: &Unit) -> FormulaResult<Self> {
        if self.logarithmic || coordinate.logarithmic {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                "logarithmic quantities cannot participate in derivatives without explicit conversion",
            ));
        }
        let source_kind = if self.temperature_kind == TemperatureKind::Absolute {
            TemperatureKind::Difference
        } else {
            self.temperature_kind
        };
        let dimension = self.dimension.checked_sub(coordinate.dimension)?;
        Ok(Self::canonical(
            dimension,
            if dimension == Dimension::TEMPERATURE {
                source_kind
            } else {
                TemperatureKind::None
            },
        ))
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.symbol)
    }
}

/// Parse common WRF/meteorological units plus deterministic SI compounds.
pub fn parse_unit(text: &str) -> FormulaResult<Unit> {
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    let direct = match lower.as_str() {
        "" | "1" | "none" | "unitless" | "dimensionless" => Some(Unit::dimensionless()),
        "%" | "percent" => Some(Unit::linear(Dimension::DIMENSIONLESS, "%", 0.01)),
        "k" | "kelvin" => Some(Unit::absolute_temperature("K", 1.0, 0.0)),
        "degc" | "c" | "celsius" | "degree_celsius" | "degrees_celsius" => {
            Some(Unit::absolute_temperature("degC", 1.0, 273.15))
        }
        "degf" | "f" | "fahrenheit" | "degree_fahrenheit" | "degrees_fahrenheit" => Some(
            Unit::absolute_temperature("degF", 5.0 / 9.0, 255.372_222_222_222_2),
        ),
        "delta_k" | "delta_kelvin" => Some(Unit::temperature_difference("delta_K", 1.0)),
        "delta_degc" | "delta_c" | "delta_celsius" => {
            Some(Unit::temperature_difference("delta_degC", 1.0))
        }
        "delta_degf" | "delta_f" | "delta_fahrenheit" => {
            Some(Unit::temperature_difference("delta_degF", 5.0 / 9.0))
        }
        "pa" => Some(Unit::linear(Dimension([-1, 1, -2, 0, 0, 0, 0]), "Pa", 1.0)),
        "hpa" | "mb" | "mbar" => Some(Unit::linear(
            Dimension([-1, 1, -2, 0, 0, 0, 0]),
            if lower == "hpa" { "hPa" } else { "mb" },
            100.0,
        )),
        "inhg" => Some(Unit::linear(
            Dimension([-1, 1, -2, 0, 0, 0, 0]),
            "inHg",
            3386.389,
        )),
        "m/s" | "m s-1" | "ms-1" | "m_s-1" | "mps" => {
            Some(Unit::linear(Dimension([1, 0, -1, 0, 0, 0, 0]), "m/s", 1.0))
        }
        "kt" | "kts" | "knot" | "knots" => Some(Unit::linear(
            Dimension([1, 0, -1, 0, 0, 0, 0]),
            "kt",
            0.514_444,
        )),
        "mph" => Some(Unit::linear(
            Dimension([1, 0, -1, 0, 0, 0, 0]),
            "mph",
            0.44704,
        )),
        "km/h" | "kph" | "kmh" => Some(Unit::linear(
            Dimension([1, 0, -1, 0, 0, 0, 0]),
            "km/h",
            1.0 / 3.6,
        )),
        "m" | "meter" | "meters" => Some(Unit::linear(Dimension::LENGTH, "m", 1.0)),
        "dam" | "dkm" | "decameter" | "decameters" => {
            Some(Unit::linear(Dimension::LENGTH, "dam", 10.0))
        }
        "km" | "kilometer" | "kilometers" => Some(Unit::linear(Dimension::LENGTH, "km", 1000.0)),
        "mm" | "millimeter" | "millimeters" => Some(Unit::linear(Dimension::LENGTH, "mm", 0.001)),
        "cm" | "centimeter" | "centimeters" => Some(Unit::linear(Dimension::LENGTH, "cm", 0.01)),
        "ft" | "foot" | "feet" => Some(Unit::linear(Dimension::LENGTH, "ft", 0.3048)),
        "mi" | "mile" | "miles" => Some(Unit::linear(Dimension::LENGTH, "mi", 1609.344)),
        "in" | "inch" | "inches" => Some(Unit::linear(Dimension::LENGTH, "in", 0.0254)),
        "s" | "sec" | "second" | "seconds" => Some(Unit::linear(Dimension::TIME, "s", 1.0)),
        "min" | "minute" | "minutes" => Some(Unit::linear(Dimension::TIME, "min", 60.0)),
        "h" | "hr" | "hour" | "hours" => Some(Unit::linear(Dimension::TIME, "h", 3600.0)),
        "kg/kg" | "kg kg-1" | "g/g" => Some(Unit::dimensionless()),
        "g/kg" | "g kg-1" => Some(Unit::linear(Dimension::DIMENSIONLESS, "g/kg", 0.001)),
        "j/kg" | "j kg-1" => Some(Unit::linear(Dimension([2, 0, -2, 0, 0, 0, 0]), "J/kg", 1.0)),
        "w/m2" | "w m-2" | "w/m^2" => Some(Unit::linear(
            Dimension([0, 1, -3, 0, 0, 0, 0]),
            "W/m^2",
            1.0,
        )),
        "pa/s" | "pa s-1" => Some(Unit::linear(
            Dimension([-1, 1, -3, 0, 0, 0, 0]),
            "Pa/s",
            1.0,
        )),
        "ub/s" | "microbar/s" | "microbars/s" => Some(Unit::linear(
            Dimension([-1, 1, -3, 0, 0, 0, 0]),
            "ub/s",
            0.1,
        )),
        "m2/s2" | "m2 s-2" | "m^2/s^2" => Some(Unit::linear(
            Dimension([2, 0, -2, 0, 0, 0, 0]),
            "m^2/s^2",
            1.0,
        )),
        "s-1" | "s^-1" | "1/s" | "/s" => {
            Some(Unit::linear(Dimension([0, 0, -1, 0, 0, 0, 0]), "s^-1", 1.0))
        }
        "10-5 s-1" | "10^-5 s^-1" | "10^-5 s-1" => Some(Unit::linear(
            Dimension([0, 0, -1, 0, 0, 0, 0]),
            "10^-5 s^-1",
            1.0e-5,
        )),
        "pvu" => Some(Unit::linear(
            Dimension([2, -1, -1, 1, 0, 0, 0]),
            "PVU",
            1.0e-6,
        )),
        "degrees" | "degree" | "deg" => Some(Unit::linear(
            Dimension::DIMENSIONLESS,
            "degrees",
            std::f64::consts::PI / 180.0,
        )),
        "rad" | "radian" | "radians" => Some(Unit::linear(Dimension::DIMENSIONLESS, "rad", 1.0)),
        "dbz" => Some(Unit {
            dimension: Dimension::DIMENSIONLESS,
            temperature_kind: TemperatureKind::None,
            symbol: "dBZ".to_string(),
            scale: 1.0,
            offset: 0.0,
            logarithmic: true,
        }),
        "mm6/m3" | "mm^6/m^3" | "mm6 m-3" => Some(Unit::linear(
            Dimension([3, 0, 0, 0, 0, 0, 0]),
            "mm^6/m^3",
            1.0e-18,
        )),
        "degc/km" | "k/km" => Some(Unit::linear(
            Dimension([-1, 0, 0, 1, 0, 0, 0]),
            trimmed,
            0.001,
        )),
        _ => None,
    };
    if let Some(unit) = direct {
        return Ok(unit);
    }
    parse_simple_si_compound(trimmed)
}

fn parse_simple_si_compound(text: &str) -> FormulaResult<Unit> {
    let normalized = text.replace('\u{00B7}', " ").replace('*', " ");
    let mut dimension = Dimension::DIMENSIONLESS;
    let mut scale = 1.0_f64;
    let mut denominator = false;
    let mut saw = false;
    for token in normalized.split_whitespace().flat_map(|part| {
        let mut pieces = Vec::new();
        for (index, piece) in part.split('/').enumerate() {
            if index > 0 {
                pieces.push("/");
            }
            if !piece.is_empty() {
                pieces.push(piece);
            }
        }
        pieces
    }) {
        if token == "/" {
            if denominator {
                return Err(FormulaError::new(
                    ErrorKind::Unit,
                    format!("ambiguous unit '{text}'"),
                ));
            }
            denominator = true;
            continue;
        }
        saw = true;
        let (base, mut exponent) = split_unit_exponent(token)?;
        if denominator {
            exponent = exponent
                .checked_neg()
                .ok_or_else(|| FormulaError::new(ErrorKind::Unit, "unit exponent overflow"))?;
        }
        let (base_dim, base_scale): (Dimension, f64) = match base {
            "m" => (Dimension::LENGTH, 1.0),
            "km" => (Dimension::LENGTH, 1000.0),
            "kg" => (Dimension::MASS, 1.0),
            "g" => (Dimension::MASS, 0.001),
            "s" => (Dimension::TIME, 1.0),
            "K" | "k" => (Dimension::TEMPERATURE, 1.0),
            "A" | "a" => (Dimension([0, 0, 0, 0, 1, 0, 0]), 1.0),
            "mol" => (Dimension([0, 0, 0, 0, 0, 1, 0]), 1.0),
            "cd" => (Dimension([0, 0, 0, 0, 0, 0, 1]), 1.0),
            other => {
                return Err(FormulaError::new(
                    ErrorKind::Unit,
                    format!("unknown unit atom '{other}' in '{text}'"),
                ));
            }
        };
        dimension = dimension.checked_add(base_dim.checked_mul(exponent)?)?;
        scale *= base_scale.powi(i32::from(exponent));
        if !scale.is_finite() || scale == 0.0 {
            return Err(FormulaError::new(ErrorKind::Unit, "unit scale overflow"));
        }
    }
    if !saw || (denominator && normalized.trim_end().ends_with('/')) {
        return Err(FormulaError::new(ErrorKind::Unit, "empty unit expression"));
    }
    if dimension.0.iter().any(|power| power.unsigned_abs() > 256) {
        return Err(FormulaError::new(
            ErrorKind::Unit,
            "compound unit exceeds final SI exponent magnitude limit 256",
        ));
    }
    if dimension == Dimension::TEMPERATURE {
        return Err(FormulaError::new(
            ErrorKind::Unit,
            "compound units that simplify to pure temperature are ambiguous; use 'K' for absolute temperature or 'delta_K' for a temperature difference",
        ));
    }
    Ok(Unit::linear(dimension, text, scale))
}

fn split_unit_exponent(token: &str) -> FormulaResult<(&str, i16)> {
    if let Some(caret) = token.find('^') {
        let base = &token[..caret];
        let exponent = token[caret + 1..].parse::<i16>().map_err(|_| {
            FormulaError::new(
                ErrorKind::Unit,
                format!("invalid unit exponent in '{token}'"),
            )
        })?;
        validate_parsed_exponent(token, exponent)?;
        return Ok((base, exponent));
    }
    let split = token
        .char_indices()
        .skip(1)
        .find(|(_, ch)| ch.is_ascii_digit() || *ch == '-')
        .map(|(index, _)| index);
    if let Some(index) = split {
        let exponent = token[index..].parse::<i16>().map_err(|_| {
            FormulaError::new(
                ErrorKind::Unit,
                format!("invalid unit exponent in '{token}'"),
            )
        })?;
        validate_parsed_exponent(token, exponent)?;
        Ok((&token[..index], exponent))
    } else {
        Ok((token, 1))
    }
}

fn validate_parsed_exponent(token: &str, exponent: i16) -> FormulaResult<()> {
    if !(-64..=64).contains(&exponent) {
        return Err(FormulaError::new(
            ErrorKind::Unit,
            format!("unit exponent in '{token}' lies outside supported range [-64, 64]"),
        ));
    }
    Ok(())
}

fn canonical_symbol(dimension: Dimension, temperature_kind: TemperatureKind) -> String {
    if dimension == Dimension::DIMENSIONLESS {
        return "1".to_string();
    }
    if dimension == Dimension::TEMPERATURE {
        return match temperature_kind {
            TemperatureKind::Absolute => "K".to_string(),
            TemperatureKind::Difference => "delta_K".to_string(),
            TemperatureKind::None => "K".to_string(),
        };
    }
    let atoms = ["m", "kg", "s", "K", "A", "mol", "cd"];
    let mut numerator = Vec::new();
    let mut denominator = Vec::new();
    for (atom, power) in atoms.iter().zip(dimension.0) {
        if power > 0 {
            numerator.push(format_power(atom, power.unsigned_abs()));
        } else if power < 0 {
            denominator.push(format_power(atom, power.unsigned_abs()));
        }
    }
    if numerator.is_empty() {
        numerator.push("1".to_string());
    }
    if denominator.is_empty() {
        numerator.join(" ")
    } else {
        format!("{}/{}", numerator.join(" "), denominator.join(" "))
    }
}

fn format_power(atom: &str, power: u16) -> String {
    if power == 1 {
        atom.to_string()
    } else {
        format!("{atom}^{power}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_affine_temperature_to_si() {
        let c = parse_unit("degC").unwrap();
        assert!((c.to_si(20.0) - 293.15).abs() < 1.0e-12);
        assert_eq!(c.temperature_kind, TemperatureKind::Absolute);
    }

    #[test]
    fn recognizes_common_wrf_derived_units() {
        assert_eq!(
            parse_unit("m2/s2").unwrap().dimension,
            Dimension([2, 0, -2, 0, 0, 0, 0])
        );
        assert_eq!(
            parse_unit("Pa/s").unwrap().dimension,
            Dimension([-1, 1, -3, 0, 0, 0, 0])
        );
    }

    #[test]
    fn derivative_of_absolute_temperature_becomes_difference() {
        let result = parse_unit("K")
            .unwrap()
            .derivative_by(&parse_unit("m").unwrap())
            .unwrap();
        assert_eq!(result.temperature_kind, TemperatureKind::None);
        assert_eq!(result.dimension, Dimension([-1, 0, 0, 1, 0, 0, 0]));
    }

    #[test]
    fn rejects_minimum_integer_unit_exponent_without_panicking() {
        assert!(parse_unit("m^-32768").is_err());
        let accumulated = std::iter::repeat("m^-64")
            .take(512)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(parse_unit(&accumulated).is_err());
    }

    #[test]
    fn pure_temperature_algebra_round_trips_as_difference() {
        let result = parse_unit("K/m")
            .unwrap()
            .multiply(&parse_unit("m").unwrap())
            .unwrap();
        assert_eq!(result.temperature_kind, TemperatureKind::Difference);
        assert_eq!(result.symbol, "delta_K");
        assert_eq!(
            parse_unit(&result.symbol).unwrap().temperature_kind,
            TemperatureKind::Difference
        );
    }

    #[test]
    fn compound_temperature_spellings_cannot_bypass_affine_semantics() {
        assert!(parse_unit("K^1").is_err());
        assert!(parse_unit("K m/m").is_err());
        assert!(parse_unit("K K/K").is_err());
        assert_eq!(
            parse_unit("K").unwrap().temperature_kind,
            TemperatureKind::Absolute
        );
        assert_eq!(
            parse_unit("delta_K").unwrap().temperature_kind,
            TemperatureKind::Difference
        );
        assert!(parse_unit("K/m").is_ok());
    }
}
