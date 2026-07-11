//! User-editable color tables and product bindings for stored variables.
//!
//! The operational resolver in `rustwx-products` remains the default source
//! of truth.  This module adds a persisted override layer above it: users can
//! bind any stored product/variable name to a custom table, and that table is
//! converted back into the same `rustwx-render` color scale types the PNG and
//! native viewers already use.

use rustwx_core::ModelId;
use rustwx_products::viewer::{
    StoreVariableStyle, UnitConvert, operational_style_for_store_variable,
};
use rustwx_render::{
    Color, ColorScale, ColormapBuildOptions, DiscreteColorScale, ExtendMode, LegendControls,
    LegendMode, LevelDensity, RenderDensity, StaticPlotStyle,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StyleOverrideSettings {
    #[serde(default)]
    pub tables: Vec<UserColorTable>,
    #[serde(default)]
    pub bindings: Vec<ProductStyleBinding>,
}

impl Default for StyleOverrideSettings {
    fn default() -> Self {
        Self {
            tables: Vec::new(),
            bindings: Vec::new(),
        }
    }
}

impl StyleOverrideSettings {
    pub fn normalized(mut self) -> Self {
        normalize_settings(&mut self);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty() && self.bindings.is_empty()
    }

    pub fn table_names(&self) -> Vec<String> {
        let mut names = self
            .tables
            .iter()
            .map(|table| table.name.clone())
            .collect::<Vec<_>>();
        names.sort_by_key(|name| name.to_ascii_lowercase());
        names
    }

    pub fn table(&self, name: &str) -> Option<&UserColorTable> {
        let wanted = normalize_table_key(name);
        self.tables
            .iter()
            .find(|table| normalize_table_key(&table.name) == wanted)
    }

    pub fn table_mut(&mut self, name: &str) -> Option<&mut UserColorTable> {
        let wanted = normalize_table_key(name);
        self.tables
            .iter_mut()
            .find(|table| normalize_table_key(&table.name) == wanted)
    }

    pub fn upsert_table(&mut self, mut table: UserColorTable) {
        table.repair();
        let key = normalize_table_key(&table.name);
        if let Some(existing) = self
            .tables
            .iter_mut()
            .find(|existing| normalize_table_key(&existing.name) == key)
        {
            *existing = table;
        } else {
            self.tables.push(table);
        }
        normalize_settings(self);
    }

    pub fn remove_table(&mut self, name: &str) {
        let wanted = normalize_table_key(name);
        self.tables
            .retain(|table| normalize_table_key(&table.name) != wanted);
        self.bindings
            .retain(|binding| normalize_table_key(&binding.table) != wanted);
    }

    pub fn binding_for_product(&self, product: &str) -> Option<&ProductStyleBinding> {
        let key = normalize_product_key(product);
        self.bindings.iter().find(|binding| binding.product == key)
    }

    pub fn bind_product(&mut self, product: &str, table: &str) {
        let product = normalize_product_key(product);
        let table = table.trim().to_string();
        if product.is_empty() || table.is_empty() {
            return;
        }
        if let Some(binding) = self
            .bindings
            .iter_mut()
            .find(|binding| binding.product == product)
        {
            binding.table = table;
        } else {
            self.bindings.push(ProductStyleBinding { product, table });
        }
        normalize_settings(self);
    }

    pub fn unbind_product(&mut self, product: &str) {
        let key = normalize_product_key(product);
        self.bindings.retain(|binding| binding.product != key);
    }

    pub fn bound_table_for_candidates<'a>(
        &'a self,
        candidates: &[String],
    ) -> Option<(&'a ProductStyleBinding, &'a UserColorTable)> {
        for candidate in candidates {
            let Some(binding) = self.binding_for_product(candidate) else {
                continue;
            };
            if let Some(table) = self.table(&binding.table) {
                return Some((binding, table));
            }
        }
        None
    }

    pub fn style_for_store_variable(
        &self,
        var_name: &str,
        stored_selector: &serde_json::Value,
        stored_units: &str,
        model: Option<ModelId>,
    ) -> Option<StoreVariableStyle> {
        let candidates = product_candidates(var_name, stored_selector);
        if let Some((_binding, table)) = self.bound_table_for_candidates(&candidates) {
            if let Some(style) = table.to_store_style(var_name, stored_units) {
                return Some(style);
            }
        }

        model.and_then(|model| {
            operational_style_for_store_variable(var_name, stored_selector, stored_units, model)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductStyleBinding {
    pub product: String,
    pub table: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserColorTable {
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub display_units: String,
    #[serde(default)]
    pub convert: UserUnitConvert,
    #[serde(default)]
    pub legend_mode: UserLegendMode,
    #[serde(default)]
    pub extend: UserExtendMode,
    #[serde(default)]
    pub mask_below: Option<f64>,
    #[serde(default)]
    pub tick_step: Option<f64>,
    pub levels: Vec<f64>,
    pub colors: Vec<[u8; 4]>,
}

impl UserColorTable {
    pub fn simple(
        name: impl Into<String>,
        title: impl Into<String>,
        units: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            title: title.into(),
            display_units: units.into(),
            convert: UserUnitConvert::None,
            legend_mode: UserLegendMode::Stepped,
            extend: UserExtendMode::Max,
            mask_below: None,
            tick_step: None,
            levels: vec![0.0, 1.0],
            colors: vec![[64, 144, 255, 255]],
        }
    }

    pub fn from_store_style(name: impl Into<String>, style: &StoreVariableStyle) -> Self {
        let discrete = style.scale.resolved_discrete();
        let mut table = Self {
            name: name.into(),
            title: style.title.clone(),
            display_units: style.display_units.clone(),
            convert: UserUnitConvert::from(style.convert),
            legend_mode: UserLegendMode::from(style.legend_mode),
            extend: UserExtendMode::from(discrete.extend),
            mask_below: discrete.mask_below,
            tick_step: style.cbar_tick_step,
            levels: discrete.levels,
            colors: discrete
                .colors
                .into_iter()
                .map(|color| [color.r, color.g, color.b, color.a])
                .collect(),
        };
        table.repair();
        table
    }

    pub fn to_store_style(
        &self,
        fallback_title: &str,
        stored_units: &str,
    ) -> Option<StoreVariableStyle> {
        let mut table = self.clone();
        table.repair();
        if table.levels.len() < 2 || table.colors.is_empty() {
            return None;
        }
        let legend_mode = LegendMode::from(table.legend_mode);
        let scale = ColorScale::Discrete(DiscreteColorScale {
            levels: table.levels,
            colors: table
                .colors
                .into_iter()
                .map(|[r, g, b, a]| Color::rgba(r, g, b, a))
                .collect(),
            extend: ExtendMode::from(table.extend),
            mask_below: table.mask_below,
        });
        Some(StoreVariableStyle {
            title: if table.title.trim().is_empty() {
                fallback_title.to_string()
            } else {
                table.title
            },
            display_units: if table.display_units.trim().is_empty() {
                stored_units.to_string()
            } else {
                table.display_units
            },
            convert: UnitConvert::from(table.convert),
            scale,
            colormap_options: ColormapBuildOptions {
                render_density: StaticPlotStyle::from_env()
                    .render_density(RenderDensity::default()),
                legend: LegendControls {
                    density: LevelDensity::default(),
                    mode: legend_mode,
                },
            },
            cbar_tick_step: table
                .tick_step
                .filter(|value| value.is_finite() && *value > 0.0),
            legend_mode,
        })
    }

    pub fn repair(&mut self) {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            self.name = "Custom table".to_string();
        }
        self.levels.retain(|value| value.is_finite());
        self.levels.sort_by(|a, b| a.total_cmp(b));
        self.levels.dedup_by(|a, b| (*a - *b).abs() < 1.0e-9);
        if self.levels.len() < 2 {
            self.levels = vec![0.0, 1.0];
        }
        let needed = self.levels.len().saturating_sub(1);
        if self.colors.is_empty() {
            self.colors.push([64, 144, 255, 255]);
        }
        while self.colors.len() < needed {
            let last = *self.colors.last().unwrap_or(&[64, 144, 255, 255]);
            self.colors.push(last);
        }
        self.colors.truncate(needed);
        if self
            .tick_step
            .is_some_and(|value| !value.is_finite() || value <= 0.0)
        {
            self.tick_step = None;
        }
        if self.mask_below.is_some_and(|value| !value.is_finite()) {
            self.mask_below = None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UserLegendMode {
    #[default]
    Stepped,
    SmoothRamp,
}

impl From<LegendMode> for UserLegendMode {
    fn from(value: LegendMode) -> Self {
        match value {
            LegendMode::Stepped => Self::Stepped,
            LegendMode::SmoothRamp => Self::SmoothRamp,
        }
    }
}

impl From<UserLegendMode> for LegendMode {
    fn from(value: UserLegendMode) -> Self {
        match value {
            UserLegendMode::Stepped => Self::Stepped,
            UserLegendMode::SmoothRamp => Self::SmoothRamp,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UserExtendMode {
    #[default]
    Neither,
    Min,
    Max,
    Both,
}

impl From<ExtendMode> for UserExtendMode {
    fn from(value: ExtendMode) -> Self {
        match value {
            ExtendMode::Neither => Self::Neither,
            ExtendMode::Min => Self::Min,
            ExtendMode::Max => Self::Max,
            ExtendMode::Both => Self::Both,
        }
    }
}

impl From<UserExtendMode> for ExtendMode {
    fn from(value: UserExtendMode) -> Self {
        match value {
            UserExtendMode::Neither => Self::Neither,
            UserExtendMode::Min => Self::Min,
            UserExtendMode::Max => Self::Max,
            UserExtendMode::Both => Self::Both,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UserUnitConvert {
    #[default]
    None,
    KelvinToFahrenheit,
    KelvinToCelsius,
    PaToHpa,
    MmToInches,
    MetersToMiles,
    PerSecondToE5PerSecond,
    MsToKnots,
    KgM3ToUgM3,
    KgM2ToMgM2,
}

impl UserUnitConvert {
    pub const ALL: [Self; 10] = [
        Self::None,
        Self::KelvinToFahrenheit,
        Self::KelvinToCelsius,
        Self::PaToHpa,
        Self::MmToInches,
        Self::MetersToMiles,
        Self::PerSecondToE5PerSecond,
        Self::MsToKnots,
        Self::KgM3ToUgM3,
        Self::KgM2ToMgM2,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::KelvinToFahrenheit => "K -> F",
            Self::KelvinToCelsius => "K -> C",
            Self::PaToHpa => "Pa -> hPa",
            Self::MmToInches => "mm -> in",
            Self::MetersToMiles => "m -> mi",
            Self::PerSecondToE5PerSecond => "s^-1 -> 1e5 s^-1",
            Self::MsToKnots => "m/s -> kt",
            Self::KgM3ToUgM3 => "kg/m3 -> ug/m3",
            Self::KgM2ToMgM2 => "kg/m2 -> mg/m2",
        }
    }
}

impl From<UnitConvert> for UserUnitConvert {
    fn from(value: UnitConvert) -> Self {
        match value {
            UnitConvert::None => Self::None,
            UnitConvert::KelvinToFahrenheit => Self::KelvinToFahrenheit,
            UnitConvert::KelvinToCelsius => Self::KelvinToCelsius,
            UnitConvert::PaToHpa => Self::PaToHpa,
            UnitConvert::MmToInches => Self::MmToInches,
            UnitConvert::MetersToMiles => Self::MetersToMiles,
            UnitConvert::PerSecondToE5PerSecond => Self::PerSecondToE5PerSecond,
            UnitConvert::MsToKnots => Self::MsToKnots,
            UnitConvert::KgM3ToUgM3 => Self::KgM3ToUgM3,
            UnitConvert::KgM2ToMgM2 => Self::KgM2ToMgM2,
        }
    }
}

impl From<UserUnitConvert> for UnitConvert {
    fn from(value: UserUnitConvert) -> Self {
        match value {
            UserUnitConvert::None => Self::None,
            UserUnitConvert::KelvinToFahrenheit => Self::KelvinToFahrenheit,
            UserUnitConvert::KelvinToCelsius => Self::KelvinToCelsius,
            UserUnitConvert::PaToHpa => Self::PaToHpa,
            UserUnitConvert::MmToInches => Self::MmToInches,
            UserUnitConvert::MetersToMiles => Self::MetersToMiles,
            UserUnitConvert::PerSecondToE5PerSecond => Self::PerSecondToE5PerSecond,
            UserUnitConvert::MsToKnots => Self::MsToKnots,
            UserUnitConvert::KgM3ToUgM3 => Self::KgM3ToUgM3,
            UserUnitConvert::KgM2ToMgM2 => Self::KgM2ToMgM2,
        }
    }
}

pub fn product_candidates(var_name: &str, stored_selector: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    push_candidate(&mut out, var_name);
    if let Some(stripped) = var_name.strip_prefix("wrf_") {
        push_candidate(&mut out, stripped);
    }
    if let Some(slug) = stored_selector
        .get("derived")
        .and_then(|value| value.as_str())
    {
        push_candidate(&mut out, slug);
        if let Some(stripped) = slug.strip_prefix("wrf_") {
            push_candidate(&mut out, stripped);
        }
    }
    out
}

pub fn normalize_product_key(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

fn normalize_table_key(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn push_candidate(out: &mut Vec<String>, value: &str) {
    let key = normalize_product_key(value);
    if !key.is_empty() && !out.contains(&key) {
        out.push(key);
    }
}

fn normalize_settings(settings: &mut StyleOverrideSettings) {
    for table in &mut settings.tables {
        table.repair();
    }
    settings
        .tables
        .sort_by_key(|table| normalize_table_key(&table.name));
    settings
        .tables
        .dedup_by(|a, b| normalize_table_key(&a.name) == normalize_table_key(&b.name));

    for binding in &mut settings.bindings {
        binding.product = normalize_product_key(&binding.product);
        binding.table = binding.table.trim().to_string();
    }
    let table_keys = settings
        .tables
        .iter()
        .map(|table| normalize_table_key(&table.name))
        .collect::<HashSet<_>>();
    settings.bindings.retain(|binding| {
        !binding.product.is_empty() && table_keys.contains(&normalize_table_key(&binding.table))
    });
    settings
        .bindings
        .sort_by_key(|binding| binding.product.clone());
    settings.bindings.dedup_by(|a, b| a.product == b.product);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_candidates_include_wrf_stripped_names() {
        let selector = serde_json::json!({"derived": "wrf_srh1"});
        assert_eq!(
            product_candidates("wrf_srh1", &selector),
            vec!["wrf_srh1", "srh1"]
        );
    }

    #[test]
    fn custom_table_builds_store_style() {
        let table = UserColorTable {
            name: "Smoke".into(),
            title: "Smoke".into(),
            display_units: "mg/m2".into(),
            convert: UserUnitConvert::None,
            legend_mode: UserLegendMode::SmoothRamp,
            extend: UserExtendMode::Max,
            mask_below: Some(0.1),
            tick_step: Some(20.0),
            levels: vec![0.0, 10.0, 20.0],
            colors: vec![[0, 0, 0, 0], [255, 128, 0, 255]],
        };
        let style = table.to_store_style("fallback", "raw").unwrap();
        assert_eq!(style.title, "Smoke");
        assert_eq!(style.display_units, "mg/m2");
        assert_eq!(style.legend_mode, LegendMode::SmoothRamp);
        assert_eq!(style.cbar_tick_step, Some(20.0));
    }
}
