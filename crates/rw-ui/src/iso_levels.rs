//! Display-time map slices synthesized from stored pressure-level volumes.
//!
//! `rw-store` keeps pressure data as compact `pressure3d` volumes for
//! soundings. The field viewer normally lists only `surface2d` variables, so
//! this naming contract exposes the classic analysis levels without changing
//! any store bytes. A synthesized slug is resolved back to its source volume
//! by the store worker when the user selects it.

/// Classic pressure surfaces exposed by the field picker.
pub const ISO_PICKER_LEVELS_HPA: [u16; 6] = [925, 850, 700, 500, 300, 250];

/// One map field that can be sliced or derived from pressure volumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsoLevelField {
    Temperature,
    Dewpoint,
    RelativeHumidity,
    WindSpeed,
    Height,
}

impl IsoLevelField {
    pub const ALL: [Self; 5] = [
        Self::Temperature,
        Self::Dewpoint,
        Self::RelativeHumidity,
        Self::WindSpeed,
        Self::Height,
    ];

    pub const fn slug_base(self) -> &'static str {
        match self {
            Self::Temperature => "temperature",
            Self::Dewpoint => "dewpoint",
            Self::RelativeHumidity => "relative_humidity",
            Self::WindSpeed => "wind_speed",
            Self::Height => "height",
        }
    }

    pub const fn label_base(self) -> &'static str {
        match self {
            Self::Temperature => "Temperature",
            Self::Dewpoint => "Dewpoint",
            Self::RelativeHumidity => "RH",
            Self::WindSpeed => "Wind speed",
            Self::Height => "Height",
        }
    }

    pub const fn source_volumes(self) -> &'static [&'static str] {
        match self {
            Self::Temperature => &["temperature_iso"],
            Self::Dewpoint => &["dewpoint_iso"],
            Self::RelativeHumidity => &["rh_iso"],
            Self::WindSpeed => &["u_iso", "v_iso"],
            Self::Height => &["height_iso"],
        }
    }
}

/// A synthesized field kind and pressure level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IsoLevelSpec {
    pub field: IsoLevelField,
    pub level_hpa: u16,
}

impl IsoLevelSpec {
    pub fn slug(self) -> String {
        format!("{}_{}", self.field.slug_base(), self.level_hpa)
    }

    pub fn label(self) -> String {
        format!("{} {} mb", self.field.label_base(), self.level_hpa)
    }
}

/// Parse an exact synthesized slug such as `temperature_850`.
pub fn parse_iso_slug(name: &str) -> Option<IsoLevelSpec> {
    IsoLevelField::ALL.iter().find_map(|&field| {
        let level_hpa = name
            .strip_prefix(field.slug_base())?
            .strip_prefix('_')?
            .parse::<u16>()
            .ok()?;
        ISO_PICKER_LEVELS_HPA
            .contains(&level_hpa)
            .then_some(IsoLevelSpec { field, level_hpa })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesized_slugs_round_trip_without_matching_real_store_names() {
        for field in IsoLevelField::ALL {
            for level_hpa in ISO_PICKER_LEVELS_HPA {
                let spec = IsoLevelSpec { field, level_hpa };
                assert_eq!(parse_iso_slug(&spec.slug()), Some(spec));
                assert!(spec.label().contains(' '));
            }
        }
        for real in [
            "temperature_2m",
            "temperature_iso",
            "temperature_850hpa",
            "wind_speed_10m",
            "height_iso",
        ] {
            assert_eq!(parse_iso_slug(real), None, "{real}");
        }
    }
}
