use egui::{
    Color32, ComboBox, DragValue, Rect, RichText, ScrollArea, Sense, Stroke, StrokeKind, TextEdit,
    Ui, pos2, vec2,
};
use rustwx_core::ModelId;
use rustwx_products::viewer::{StoreVariableStyleTemplate, operational_style_templates};
use std::collections::HashMap;

use crate::style_overrides::{
    StyleOverrideSettings, UserColorTable, UserExtendMode, UserLegendMode, UserUnitConvert,
    normalize_product_key,
};
use crate::worker::FieldData;

#[derive(Debug)]
pub struct ColorTableEditorPanel {
    settings: StyleOverrideSettings,
    selected_table: Option<String>,
    templates: Vec<StoreVariableStyleTemplate>,
    selected_template: Option<String>,
    template_filter: String,
    template_preview_cache: HashMap<String, UserColorTable>,
    last_current_product: Option<String>,
    status: Option<String>,
    changed: bool,
}

impl Default for ColorTableEditorPanel {
    fn default() -> Self {
        let templates = operational_style_templates(ModelId::Hrrr);
        let selected_template = templates.first().map(|template| template.id.clone());
        Self {
            settings: StyleOverrideSettings::default(),
            selected_table: None,
            templates,
            selected_template,
            template_filter: String::new(),
            template_preview_cache: HashMap::new(),
            last_current_product: None,
            status: None,
            changed: false,
        }
    }
}

impl ColorTableEditorPanel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_settings(&mut self, settings: StyleOverrideSettings) {
        self.settings = settings.normalized();
        if self
            .selected_table
            .as_ref()
            .is_none_or(|name| self.settings.table(name).is_none())
        {
            self.selected_table = self.settings.tables.first().map(|table| table.name.clone());
        }
        self.changed = false;
    }

    pub fn settings(&self) -> &StyleOverrideSettings {
        &self.settings
    }

    pub fn take_changed(&mut self) -> bool {
        let changed = self.changed;
        self.changed = false;
        changed
    }

    pub fn ui(&mut self, ui: &mut Ui, current_field: Option<&FieldData>) {
        ui.vertical(|ui| {
            self.current_product_ui(ui, current_field);
            ui.separator();
            self.table_picker_ui(ui, current_field);
            ui.separator();
            self.table_editor_ui(ui);
        });
    }

    fn current_product_ui(&mut self, ui: &mut Ui, current_field: Option<&FieldData>) {
        ui.label(RichText::new("Current Product").small().strong());
        let Some(field) = current_field else {
            ui.label(RichText::new("Load a variable to bind its color table.").weak());
            return;
        };

        let product = normalize_product_key(&field.key.var);
        if self.last_current_product.as_deref() != Some(product.as_str()) {
            self.last_current_product = Some(product.clone());
            if let Some(template_id) = self.best_template_for_field(&product, field) {
                self.selected_template = Some(template_id);
            }
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(&product);
            ui.label(RichText::new(&field.units).small().weak());
            ui.label("Map uses");
            let mut picked = self
                .settings
                .binding_for_product(&product)
                .map(|binding| binding.table.clone())
                .unwrap_or_default();
            ComboBox::from_id_salt("rw-ui-current-product-table")
                .selected_text(if picked.is_empty() {
                    "Operational/default"
                } else {
                    &picked
                })
                .width(180.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut picked, String::new(), "Operational/default");
                    for name in self.settings.table_names() {
                        ui.selectable_value(&mut picked, name.clone(), name);
                    }
                });
            let previous = self
                .settings
                .binding_for_product(&product)
                .map(|binding| binding.table.clone())
                .unwrap_or_default();
            if picked != previous {
                if picked.is_empty() {
                    self.settings.unbind_product(&product);
                    self.status = Some(format!("{product} uses the operational default"));
                } else {
                    self.settings.bind_product(&product, &picked);
                    self.status = Some(format!("{product} uses {picked}"));
                }
                self.changed = true;
            }
            if ui
                .add_enabled(
                    self.settings.binding_for_product(&product).is_some(),
                    egui::Button::new("Back to default"),
                )
                .clicked()
            {
                self.settings.unbind_product(&product);
                self.status = Some(format!("{product} uses the operational default"));
                self.changed = true;
            }
        });
        if let Some(table) = self.current_product_preview_table(&product, field) {
            palette_preview(ui, "Current map palette", &table);
        }

        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Pick palette").small().strong());
            self.template_picker(ui, "current-product-template");
        });
        if let Some(preview) = self.selected_template_preview().cloned() {
            palette_preview(ui, "Selected built-in palette", &preview);
        }
        ui.horizontal_wrapped(|ui| {
            let can_create = self.selected_template().is_some();
            let can_use_colors = can_create && field.style.is_some();
            if ui
                .add_enabled(can_use_colors, egui::Button::new("Use colors only"))
                .on_hover_text(
                    "Resample these colors onto the current map's intervals. Keeps the current scale, display units, conversion, legend, mask, and tick settings, then saves and binds an editable preset.",
                )
                .clicked()
            {
                if let Some(template) = self.selected_template().cloned() {
                    self.apply_template_colors_only(&template, &product, field);
                }
            }
            if ui
                .add_enabled(
                    can_create,
                    egui::Button::new("Apply full preset (scale + units)"),
                )
                .on_hover_text(
                    "Replace the complete map style with this built-in preset, including its levels, display units, unit conversion, legend, mask, and tick settings.",
                )
                .clicked()
            {
                if let Some(template) = self.selected_template().cloned() {
                    self.create_table_from_template(&template, Some(&product));
                }
            }
            if ui
                .add_enabled(can_create, egui::Button::new("Save copy"))
                .on_hover_text("Save the complete built-in preset without applying it to the map.")
                .clicked()
            {
                if let Some(template) = self.selected_template().cloned() {
                    self.create_table_from_template(&template, None);
                }
            }
            if ui
                .add_enabled(
                    field.style.is_some(),
                    egui::Button::new("Edit current palette"),
                )
                .clicked()
            {
                if let Some(style) = &field.style {
                    let name = unique_table_name(&self.settings, &format!("{} custom", product));
                    let table = UserColorTable::from_store_style(name.clone(), style);
                    self.settings.upsert_table(table);
                    self.settings.bind_product(&product, &name);
                    self.selected_table = Some(name);
                    self.status = Some("Editing current map palette".to_string());
                    self.changed = true;
                }
            }
        });
    }

    fn table_picker_ui(&mut self, ui: &mut Ui, current_field: Option<&FieldData>) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Edit saved preset").small().strong());
            let names = self.settings.table_names();
            let selected = self
                .selected_table
                .clone()
                .or_else(|| names.first().cloned())
                .unwrap_or_else(|| "no saved presets".to_string());
            ComboBox::from_id_salt("rw-ui-color-table-picker")
                .selected_text(&selected)
                .width(220.0)
                .show_ui(ui, |ui| {
                    for name in &names {
                        if ui
                            .selectable_label(
                                self.selected_table.as_deref() == Some(name.as_str()),
                                name,
                            )
                            .clicked()
                        {
                            self.selected_table = Some(name.clone());
                        }
                    }
                });
            let can_apply = current_field.is_some() && self.selected_table.is_some();
            if ui
                .add_enabled(can_apply, egui::Button::new("Use preset as-is"))
                .on_hover_text(
                    "Bind the complete saved preset to this map, including its scale and unit conversion.",
                )
                .clicked()
            {
                if let (Some(field), Some(table)) = (current_field, self.selected_table.clone()) {
                    let product = normalize_product_key(&field.key.var);
                    self.settings.bind_product(&product, &table);
                    self.status = Some(format!("Applied {table} to {product}"));
                    self.changed = true;
                }
            }
            let fit_availability =
                fit_scale_availability(&self.settings, self.selected_table.as_deref(), current_field);
            let fit_help = match fit_availability {
                Ok(()) => "Evenly redistribute this preset's existing levels over the exact finite range of the currently displayed values. Outliers are included. Colors, units, conversion, legend, extend mode, and mask are preserved; a stale fixed tick step is cleared.",
                Err(reason) => reason.help(),
            };
            if ui
                .add_enabled(
                    fit_availability.is_ok(),
                    egui::Button::new("Fit scale to full range"),
                )
                .on_hover_text(fit_help)
                .clicked()
            {
                if let (Some(field), Some(table_name)) =
                    (current_field, self.selected_table.clone())
                {
                    self.fit_selected_table_to_field(&table_name, field);
                }
            }
            if ui.button("New blank").clicked() {
                let name = unique_table_name(&self.settings, "Custom table");
                self.settings
                    .upsert_table(UserColorTable::simple(&name, &name, ""));
                self.selected_table = Some(name);
                self.status = Some("Saved blank preset".to_string());
                self.changed = true;
            }
            if ui
                .add_enabled(
                    self.selected_table.is_some(),
                    egui::Button::new("Duplicate"),
                )
                .clicked()
            {
                if let Some(name) = self.selected_table.clone() {
                    if let Some(source) = self.settings.table(&name).cloned() {
                        let mut copy = source;
                        copy.name = unique_table_name(&self.settings, &format!("{} copy", name));
                        let selected = copy.name.clone();
                        self.settings.upsert_table(copy);
                        self.selected_table = Some(selected);
                        self.status = Some("Saved duplicate preset".to_string());
                        self.changed = true;
                    }
                }
            }
            if ui
                .add_enabled(self.selected_table.is_some(), egui::Button::new("Delete"))
                .clicked()
            {
                if let Some(name) = self.selected_table.take() {
                    self.settings.remove_table(&name);
                    self.selected_table =
                        self.settings.tables.first().map(|table| table.name.clone());
                    self.status = Some("Deleted preset".to_string());
                    self.changed = true;
                }
            }
            if ui
                .add_enabled(
                    self.selected_table.is_some() && self.selected_template().is_some(),
                    egui::Button::new("Reset preset"),
                )
                .clicked()
            {
                if let (Some(name), Some(template)) = (
                    self.selected_table.clone(),
                    self.selected_template().cloned(),
                ) {
                    self.reset_table_from_template(&name, &template);
                }
            }
        });
        if let Some(status) = &self.status {
            ui.label(RichText::new(status).small().weak());
        }
        ui.label(RichText::new("Saved automatically").small().weak());
    }

    fn template_picker(&mut self, ui: &mut Ui, id: &'static str) {
        ui.add(
            TextEdit::singleline(&mut self.template_filter)
                .desired_width(120.0)
                .hint_text("filter"),
        );
        let selected_text = self
            .selected_template()
            .map(template_label)
            .unwrap_or_else(|| "Template".to_string());
        ComboBox::from_id_salt(id)
            .selected_text(selected_text)
            .width(260.0)
            .show_ui(ui, |ui| {
                let filter = normalize_product_key(&self.template_filter);
                for template in &self.templates {
                    if !template_matches_filter(template, &filter) {
                        continue;
                    }
                    let label = template_label(template);
                    ui.selectable_value(
                        &mut self.selected_template,
                        Some(template.id.clone()),
                        label,
                    );
                }
            });
    }

    fn selected_template(&self) -> Option<&StoreVariableStyleTemplate> {
        let id = self.selected_template.as_deref()?;
        self.templates.iter().find(|template| template.id == id)
    }

    fn selected_template_preview(&mut self) -> Option<&UserColorTable> {
        let template = self.selected_template()?.clone();
        let id = template.id.clone();
        let preview = self
            .template_preview_cache
            .entry(id)
            .or_insert_with(|| UserColorTable::from_store_style("preview", &template.style));
        Some(preview)
    }

    fn best_template_for_field(&self, product: &str, field: &FieldData) -> Option<String> {
        let title_key = field
            .style
            .as_ref()
            .map(|style| normalize_product_key(&style.title));
        if let Some(title_key) = title_key.as_deref() {
            if let Some(template) = self
                .templates
                .iter()
                .find(|template| normalize_product_key(&template.style.title) == title_key)
            {
                return Some(template.id.clone());
            }
            if let Some(template) = self
                .templates
                .iter()
                .find(|template| normalize_product_key(&template.label) == title_key)
            {
                return Some(template.id.clone());
            }
        }
        self.templates
            .iter()
            .find(|template| normalize_product_key(&template.slug) == product)
            .map(|template| template.id.clone())
    }

    fn current_product_preview_table(
        &self,
        product: &str,
        field: &FieldData,
    ) -> Option<UserColorTable> {
        if let Some(binding) = self.settings.binding_for_product(product) {
            if let Some(table) = self.settings.table(&binding.table) {
                return Some(table.clone());
            }
        }
        field
            .style
            .as_ref()
            .map(|style| UserColorTable::from_store_style("Operational/default", style))
    }

    fn create_table_from_template(
        &mut self,
        template: &StoreVariableStyleTemplate,
        bind_product: Option<&str>,
    ) {
        let base_name = match bind_product {
            Some(product) => format!("{product} from {}", template.label),
            None => template.label.clone(),
        };
        let name = unique_table_name(&self.settings, &base_name);
        let mut table = UserColorTable::from_store_style(name.clone(), &template.style);
        table.title = template.label.clone();
        self.settings.upsert_table(table);
        if let Some(product) = bind_product {
            self.settings.bind_product(product, &name);
            self.status = Some(format!("Applied {} to {product}", template.label));
        } else {
            self.status = Some(format!("Saved preset {name}"));
        }
        self.selected_table = Some(name);
        self.changed = true;
    }

    fn apply_template_colors_only(
        &mut self,
        template: &StoreVariableStyleTemplate,
        product: &str,
        field: &FieldData,
    ) {
        let Some(style) = field.style.as_ref() else {
            self.status = Some(
                "Colors were not applied: the current field has no scale to preserve.".to_string(),
            );
            return;
        };
        let source = UserColorTable::from_store_style("built-in colors", &template.style);
        let name = editable_table_name(
            &self.settings,
            product,
            &format!("{product} custom colors"),
        );
        let current = UserColorTable::from_store_style(name.clone(), style);
        let Some(table) = with_resampled_colors(current, &source.colors) else {
            self.status = Some(format!(
                "Colors were not applied: {} has no usable colors.",
                template.label
            ));
            return;
        };
        let interval_count = table.colors.len();
        self.settings.upsert_table(table);
        self.settings.bind_product(product, &name);
        self.selected_table = Some(name.clone());
        self.status = Some(format!(
            "Used {} colors for {product} across {interval_count} existing intervals; scale and units were kept in {name}",
            template.label
        ));
        self.changed = true;
    }

    fn fit_selected_table_to_field(&mut self, table_name: &str, field: &FieldData) {
        if let Err(reason) =
            fit_scale_availability(&self.settings, Some(table_name), Some(field))
        {
            self.status = Some(format!("Scale was not changed: {}", reason.help()));
            return;
        }
        let Some(mut table) = self.settings.table(table_name).cloned() else {
            self.status = Some("Scale was not changed: the selected preset is missing.".to_string());
            return;
        };
        match fit_table_to_values(&mut table, &field.values) {
            Ok(outcome) => {
                self.settings.upsert_table(table);
                self.status = Some(if outcome.constant {
                    format!(
                        "Fitted {table_name} around the constant displayed value {}; colors and units were kept and the fixed tick step was cleared",
                        format_number(outcome.source_min)
                    )
                } else {
                    format!(
                        "Fitted {table_name} to the full displayed range {} to {}; colors and units were kept and the fixed tick step was cleared",
                        format_number(outcome.source_min),
                        format_number(outcome.source_max)
                    )
                });
                self.changed = true;
            }
            Err(FitScaleError::NoFiniteValues) => {
                self.status = Some(
                    "Scale was not changed: the current field has no finite displayed values."
                        .to_string(),
                );
            }
            Err(FitScaleError::TooFewLevels) => {
                self.status = Some(
                    "Scale was not changed: the preset needs at least two levels.".to_string(),
                );
            }
        }
    }

    fn reset_table_from_template(&mut self, name: &str, template: &StoreVariableStyleTemplate) {
        let Some(existing) = self.settings.table(name).cloned() else {
            return;
        };
        let mut table = UserColorTable::from_store_style(existing.name.clone(), &template.style);
        table.title = template.label.clone();
        self.settings.upsert_table(table);
        self.selected_table = Some(existing.name.clone());
        self.status = Some(format!("Reset {} from {}", existing.name, template.label));
        self.changed = true;
    }

    fn table_editor_ui(&mut self, ui: &mut Ui) {
        let Some(selected) = self.selected_table.clone() else {
            ui.label(RichText::new("No custom tables yet.").weak());
            return;
        };
        let Some(index) = self
            .settings
            .tables
            .iter()
            .position(|table| table.name == selected)
        else {
            ui.label(RichText::new("Selected table no longer exists.").weak());
            return;
        };

        let old_name = self.settings.tables[index].name.clone();
        let mut table_changed = false;
        {
            let table = &mut self.settings.tables[index];
            palette_preview(ui, "Preset preview", table);
            ui.horizontal_wrapped(|ui| {
                ui.label("Name");
                table_changed |= ui
                    .add(TextEdit::singleline(&mut table.name).desired_width(180.0))
                    .changed();
                ui.label("Title");
                table_changed |= ui
                    .add(TextEdit::singleline(&mut table.title).desired_width(180.0))
                    .changed();
                ui.label("Units");
                table_changed |= ui
                    .add(TextEdit::singleline(&mut table.display_units).desired_width(90.0))
                    .changed();
            });
            ui.horizontal_wrapped(|ui| {
                ComboBox::from_id_salt("rw-ui-table-convert")
                    .selected_text(table.convert.label())
                    .width(130.0)
                    .show_ui(ui, |ui| {
                        for convert in UserUnitConvert::ALL {
                            table_changed |= ui
                                .selectable_value(&mut table.convert, convert, convert.label())
                                .changed();
                        }
                    });
                ComboBox::from_id_salt("rw-ui-table-legend-mode")
                    .selected_text(match table.legend_mode {
                        UserLegendMode::Stepped => "Stepped",
                        UserLegendMode::SmoothRamp => "Smooth ramp",
                    })
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        table_changed |= ui
                            .selectable_value(
                                &mut table.legend_mode,
                                UserLegendMode::Stepped,
                                "Stepped",
                            )
                            .changed();
                        table_changed |= ui
                            .selectable_value(
                                &mut table.legend_mode,
                                UserLegendMode::SmoothRamp,
                                "Smooth ramp",
                            )
                            .changed();
                    });
                ComboBox::from_id_salt("rw-ui-table-extend")
                    .selected_text(match table.extend {
                        UserExtendMode::Neither => "No extend",
                        UserExtendMode::Min => "Extend min",
                        UserExtendMode::Max => "Extend max",
                        UserExtendMode::Both => "Extend both",
                    })
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for (mode, label) in [
                            (UserExtendMode::Neither, "No extend"),
                            (UserExtendMode::Min, "Extend min"),
                            (UserExtendMode::Max, "Extend max"),
                            (UserExtendMode::Both, "Extend both"),
                        ] {
                            table_changed |= ui
                                .selectable_value(&mut table.extend, mode, label)
                                .changed();
                        }
                    });
                let mut tick = table.tick_step.unwrap_or(0.0);
                if ui
                    .add(DragValue::new(&mut tick).speed(0.25).prefix("tick "))
                    .changed()
                {
                    table.tick_step = (tick > 0.0 && tick.is_finite()).then_some(tick);
                    table_changed = true;
                }
                let mut has_mask = table.mask_below.is_some();
                if ui.checkbox(&mut has_mask, "mask below").changed() {
                    table.mask_below = has_mask.then_some(0.0);
                    table_changed = true;
                }
                if has_mask {
                    let mut mask = table.mask_below.unwrap_or(0.0);
                    if ui
                        .add(DragValue::new(&mut mask).speed(0.25).prefix("< "))
                        .changed()
                    {
                        table.mask_below = mask.is_finite().then_some(mask);
                        table_changed = true;
                    }
                }
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Levels and Colors").small().strong());
                if ui.button("Add interval").clicked() {
                    let last = *table.levels.last().unwrap_or(&1.0);
                    let prev = table
                        .levels
                        .get(table.levels.len().saturating_sub(2))
                        .copied()
                        .unwrap_or(last - 1.0);
                    table.levels.push(last + (last - prev).abs().max(1.0));
                    table
                        .colors
                        .push(*table.colors.last().unwrap_or(&[255, 255, 255, 255]));
                    table_changed = true;
                }
                if ui.button("Repair/sort").clicked() {
                    table.repair();
                    table_changed = true;
                }
            });
            ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                let interval_count = table.levels.len().saturating_sub(1);
                let mut remove_interval = None;
                for index in 0..interval_count {
                    ui.horizontal(|ui| {
                        ui.label(format!("{index:02}"));
                        table_changed |= ui
                            .add(
                                DragValue::new(&mut table.levels[index])
                                    .speed(0.5)
                                    .prefix("from "),
                            )
                            .changed();
                        let mut color = color32_from_rgba(table.colors[index]);
                        if egui::color_picker::color_edit_button_srgba(
                            ui,
                            &mut color,
                            egui::color_picker::Alpha::BlendOrAdditive,
                        )
                        .changed()
                        {
                            table.colors[index] = rgba_from_color32(color);
                            table_changed = true;
                        }
                        if ui
                            .add_enabled(interval_count > 1, egui::Button::new("Remove"))
                            .clicked()
                        {
                            remove_interval = Some(index);
                        }
                    });
                }
                ui.horizontal(|ui| {
                    ui.label("end");
                    if let Some(last) = table.levels.last_mut() {
                        table_changed |= ui
                            .add(DragValue::new(last).speed(0.5).prefix("to "))
                            .changed();
                    }
                });
                if let Some(index) = remove_interval {
                    if index < table.colors.len() {
                        table.colors.remove(index);
                    }
                    if index + 1 < table.levels.len() {
                        table.levels.remove(index + 1);
                    }
                    table_changed = true;
                }
            });
        }

        if table_changed {
            let new_name = self.settings.tables[index].name.trim().to_string();
            if !new_name.is_empty() && new_name != old_name {
                for binding in &mut self.settings.bindings {
                    if binding.table == old_name {
                        binding.table = new_name.clone();
                    }
                }
                self.selected_table = Some(new_name);
            }
            self.settings.tables[index].repair();
            self.settings = self.settings.clone().normalized();
            self.changed = true;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FitScaleUnavailable {
    NoCurrentField,
    NoSelectedPreset,
    MissingPreset,
    PresetNotBound,
    NoCurrentStyle,
    UnsafeUnitContext,
    NoFiniteValues,
}

impl FitScaleUnavailable {
    fn help(self) -> &'static str {
        match self {
            Self::NoCurrentField => "Load or generate a field before fitting a scale.",
            Self::NoSelectedPreset => "Select a saved preset before fitting its scale.",
            Self::MissingPreset => "The selected preset no longer exists.",
            Self::PresetNotBound => {
                "Apply the selected saved preset to the current map before fitting it. This keeps the displayed value units unambiguous."
            }
            Self::NoCurrentStyle => {
                "The current field has no resolved display scale. Use a full preset first."
            }
            Self::UnsafeUnitContext => {
                "Wait for the map to refresh after changing this preset's units or conversion; fitting is disabled while the displayed values and preset units differ."
            }
            Self::NoFiniteValues => {
                "The current field has no finite displayed values, so there is no range to fit."
            }
        }
    }
}

fn fit_scale_availability(
    settings: &StyleOverrideSettings,
    selected_table: Option<&str>,
    current_field: Option<&FieldData>,
) -> Result<(), FitScaleUnavailable> {
    let field = current_field.ok_or(FitScaleUnavailable::NoCurrentField)?;
    let selected = selected_table.ok_or(FitScaleUnavailable::NoSelectedPreset)?;
    let table = settings
        .table(selected)
        .ok_or(FitScaleUnavailable::MissingPreset)?;
    let product = normalize_product_key(&field.key.var);
    let binding = settings
        .binding_for_product(&product)
        .ok_or(FitScaleUnavailable::PresetNotBound)?;
    if !binding.table.trim().eq_ignore_ascii_case(selected.trim()) {
        return Err(FitScaleUnavailable::PresetNotBound);
    }
    let style = field
        .style
        .as_ref()
        .ok_or(FitScaleUnavailable::NoCurrentStyle)?;
    let expected_convert = rustwx_products::viewer::UnitConvert::from(table.convert);
    let configured_units_match = table.display_units.trim().is_empty()
        || table.display_units == style.display_units;
    if field.units != style.display_units
        || style.convert != expected_convert
        || !configured_units_match
    {
        return Err(FitScaleUnavailable::UnsafeUnitContext);
    }
    finite_display_range(&field.values).ok_or(FitScaleUnavailable::NoFiniteValues)?;
    Ok(())
}

fn editable_table_name(
    settings: &StyleOverrideSettings,
    product: &str,
    fallback_base: &str,
) -> String {
    if let Some(existing) = settings
        .binding_for_product(product)
        .and_then(|binding| settings.table(&binding.table))
    {
        return existing.name.clone();
    }
    unique_table_name(settings, fallback_base)
}

/// Replace only the RGBA sequence. All scale, unit, conversion, legend, mask,
/// tick, title, and extend fields remain byte-for-byte unchanged.
fn with_resampled_colors(
    mut current: UserColorTable,
    palette: &[[u8; 4]],
) -> Option<UserColorTable> {
    let interval_count = current.levels.len().checked_sub(1)?;
    if interval_count == 0 {
        return None;
    }
    current.colors = resample_palette_colors(palette, interval_count)?;
    Some(current)
}

/// Linearly sample RGBA channels at evenly spaced positions. The two endpoints
/// are exact when at least two colors are requested; a one-color result uses
/// the deterministic lower-middle source color.
fn resample_palette_colors(source: &[[u8; 4]], count: usize) -> Option<Vec<[u8; 4]>> {
    if source.is_empty() || count == 0 {
        return None;
    }
    if count == 1 {
        return Some(vec![source[(source.len() - 1) / 2]]);
    }
    if source.len() == 1 {
        return Some(vec![source[0]; count]);
    }

    let denominator = (count - 1) as u128;
    let source_span = (source.len() - 1) as u128;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let numerator = (index as u128) * source_span;
        let left = (numerator / denominator) as usize;
        let remainder = numerator % denominator;
        let right = (left + 1).min(source.len() - 1);
        let mut color = [0_u8; 4];
        for channel in 0..4 {
            let left_weight = denominator - remainder;
            let weighted = u128::from(source[left][channel]) * left_weight
                + u128::from(source[right][channel]) * remainder;
            color[channel] = ((weighted + denominator / 2) / denominator) as u8;
        }
        out.push(color);
    }
    Some(out)
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct FitScaleOutcome {
    source_min: f64,
    source_max: f64,
    constant: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FitScaleError {
    NoFiniteValues,
    TooFewLevels,
}

fn fit_table_to_values(
    table: &mut UserColorTable,
    values: &[f32],
) -> Result<FitScaleOutcome, FitScaleError> {
    if table.levels.len() < 2 {
        return Err(FitScaleError::TooFewLevels);
    }
    let (source_min, source_max) =
        finite_display_range(values).ok_or(FitScaleError::NoFiniteValues)?;
    let constant = source_min == source_max;
    let (scale_min, scale_max) = if constant {
        padded_constant_range(source_min)
    } else {
        (source_min, source_max)
    };
    let last = table.levels.len() - 1;
    for (index, level) in table.levels.iter_mut().enumerate() {
        *level = if index == 0 {
            scale_min
        } else if index == last {
            scale_max
        } else {
            scale_min + (scale_max - scale_min) * index as f64 / last as f64
        };
    }
    // A user-specified tick step describes the old scale and can become
    // nonsensical after fitting. Auto ticks are deterministic for the new one.
    table.tick_step = None;
    Ok(FitScaleOutcome {
        source_min,
        source_max,
        constant,
    })
}

fn finite_display_range(values: &[f32]) -> Option<(f64, f64)> {
    let mut range: Option<(f64, f64)> = None;
    for value in values.iter().copied().filter(|value| value.is_finite()) {
        let value = f64::from(value);
        range = Some(match range {
            Some((min, max)) => (min.min(value), max.max(value)),
            None => (value, value),
        });
    }
    range
}

fn padded_constant_range(center: f64) -> (f64, f64) {
    if center == 0.0 {
        return (-1.0, 1.0);
    }
    let padding = (center.abs() * 0.05).max(1.0e-6);
    (center - padding, center + padding)
}

fn color32_from_rgba([r, g, b, a]: [u8; 4]) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

fn rgba_from_color32(color: Color32) -> [u8; 4] {
    [color.r(), color.g(), color.b(), color.a()]
}

fn palette_preview(ui: &mut Ui, label: &str, table: &UserColorTable) {
    ui.vertical(|ui| {
        ui.label(RichText::new(label).small().strong());
        let width = ui.available_width().clamp(240.0, 620.0);
        let height = 24.0;
        let (rect, _response) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
        let painter = ui.painter_at(rect.expand(1.0));
        let levels = &table.levels;
        let colors = &table.colors;
        if levels.len() >= 2 && !colors.is_empty() {
            let min = levels.first().copied().unwrap_or(0.0);
            let max = levels.last().copied().unwrap_or(min + 1.0);
            let span = (max - min).abs().max(f64::EPSILON);
            for (index, color) in colors.iter().enumerate() {
                let left_level = levels.get(index).copied().unwrap_or(min);
                let right_level = levels.get(index + 1).copied().unwrap_or(max);
                let x0 = rect.left() + (((left_level - min) / span) as f32) * rect.width();
                let x1 = rect.left() + (((right_level - min) / span) as f32) * rect.width();
                let segment = Rect::from_min_max(
                    pos2(x0.min(x1), rect.top()),
                    pos2(x0.max(x1).max(x0.min(x1) + 1.0), rect.bottom()),
                );
                painter.rect_filled(segment, 0.0, color32_from_rgba(*color));
            }
            ui.horizontal(|ui| {
                ui.label(RichText::new(format_number(min)).small().weak());
                ui.add_space((width - 140.0).max(0.0));
                ui.label(RichText::new(format_number(max)).small().weak());
            });
        } else {
            painter.rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
            ui.label(RichText::new("No colors").small().weak());
        }
        painter.rect_stroke(
            rect,
            0.0,
            Stroke::new(1.0, ui.visuals().weak_text_color()),
            StrokeKind::Outside,
        );
    });
}

fn format_number(value: f64) -> String {
    if value.abs() >= 100.0 {
        format!("{value:.0}")
    } else if value.abs() >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

fn unique_table_name(settings: &StyleOverrideSettings, base: &str) -> String {
    let base = base.trim();
    let base = if base.is_empty() {
        "Custom table"
    } else {
        base
    };
    if settings.table(base).is_none() {
        return base.to_string();
    }
    for index in 2..10_000 {
        let candidate = format!("{base} {index}");
        if settings.table(&candidate).is_none() {
            return candidate;
        }
    }
    format!("{base} copy")
}

fn template_label(template: &StoreVariableStyleTemplate) -> String {
    format!(
        "{} - {} ({})",
        template.category, template.label, template.slug
    )
}

fn template_matches_filter(template: &StoreVariableStyleTemplate, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    normalize_product_key(&template.label).contains(filter)
        || normalize_product_key(&template.slug).contains(filter)
        || normalize_product_key(&template.category).contains(filter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::{FieldKey, HourKey};

    fn sample_table() -> UserColorTable {
        UserColorTable {
            name: "Custom wind".to_string(),
            title: "Formula wind".to_string(),
            display_units: "m/s".to_string(),
            convert: UserUnitConvert::None,
            legend_mode: UserLegendMode::SmoothRamp,
            extend: UserExtendMode::Both,
            mask_below: Some(0.25),
            tick_step: Some(2.5),
            levels: vec![-10.0, 0.0, 10.0, 20.0, 30.0],
            colors: vec![
                [1, 2, 3, 4],
                [5, 6, 7, 8],
                [9, 10, 11, 12],
                [13, 14, 15, 16],
            ],
        }
    }

    #[test]
    fn palette_resampling_is_deterministic_and_keeps_endpoints() {
        let source = [
            [0, 10, 20, 30],
            [100, 110, 120, 130],
            [200, 210, 220, 230],
        ];
        assert_eq!(
            resample_palette_colors(&source, 5),
            Some(vec![
                [0, 10, 20, 30],
                [50, 60, 70, 80],
                [100, 110, 120, 130],
                [150, 160, 170, 180],
                [200, 210, 220, 230],
            ])
        );
        assert_eq!(
            resample_palette_colors(&source, 1),
            Some(vec![[100, 110, 120, 130]])
        );
        assert_eq!(resample_palette_colors(&[], 4), None);
        assert_eq!(resample_palette_colors(&source, 0), None);
    }

    #[test]
    fn colors_only_preserves_scale_units_conversion_and_legend_controls() {
        let current = sample_table();
        let palette = [
            [0, 0, 255, 255],
            [255, 255, 255, 255],
            [255, 0, 0, 255],
        ];
        let recolored = with_resampled_colors(current.clone(), &palette).unwrap();
        let mut expected = current;
        expected.colors = vec![
            [0, 0, 255, 255],
            [170, 170, 255, 255],
            [255, 170, 170, 255],
            [255, 0, 0, 255],
        ];
        assert_eq!(recolored, expected);
    }

    #[test]
    fn temperature_palette_colors_cannot_change_a_wind_fields_units() {
        let wind = sample_table();
        let mut temperature_palette = sample_table();
        temperature_palette.display_units = "degF".to_string();
        temperature_palette.convert = UserUnitConvert::KelvinToFahrenheit;
        temperature_palette.colors = vec![[20, 40, 200, 255], [240, 30, 20, 255]];

        let recolored =
            with_resampled_colors(wind.clone(), &temperature_palette.colors).unwrap();
        assert_eq!(recolored.display_units, "m/s");
        assert_eq!(recolored.convert, UserUnitConvert::None);
        assert_eq!(recolored.levels, wind.levels);
        assert_eq!(recolored.mask_below, wind.mask_below);
        assert_eq!(recolored.tick_step, wind.tick_step);
    }

    #[test]
    fn full_range_fit_includes_exact_outliers_and_preserves_other_style() {
        let mut fitted = sample_table();
        let before = fitted.clone();
        let outcome = fit_table_to_values(
            &mut fitted,
            &[f32::NAN, -999.0, 0.0, 1.0, 999.0, f32::INFINITY],
        )
        .unwrap();

        assert_eq!(outcome.source_min, -999.0);
        assert_eq!(outcome.source_max, 999.0);
        assert!(!outcome.constant);
        assert_eq!(
            fitted.levels,
            vec![-999.0, -499.5, 0.0, 499.5, 999.0]
        );
        assert_eq!(fitted.colors, before.colors);
        assert_eq!(fitted.name, before.name);
        assert_eq!(fitted.title, before.title);
        assert_eq!(fitted.display_units, before.display_units);
        assert_eq!(fitted.convert, before.convert);
        assert_eq!(fitted.legend_mode, before.legend_mode);
        assert_eq!(fitted.extend, before.extend);
        assert_eq!(fitted.mask_below, before.mask_below);
        assert_eq!(fitted.tick_step, None);
    }

    #[test]
    fn full_range_fit_handles_constant_and_all_zero_fields_deterministically() {
        for center in [0.0_f32, 7.0_f32] {
            let mut first = sample_table();
            let mut second = sample_table();
            let values = [center, center, f32::NAN];
            let first_outcome = fit_table_to_values(&mut first, &values).unwrap();
            let _second_outcome = fit_table_to_values(&mut second, &values).unwrap();
            assert!(first_outcome.constant);
            assert_eq!(first, second);
            let expected = if center == 0.0 {
                (-1.0, 1.0)
            } else {
                (6.65, 7.35)
            };
            assert_eq!(first.levels.first(), Some(&expected.0));
            assert_eq!(first.levels.last(), Some(&expected.1));
            assert!(first.levels.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }

    #[test]
    fn full_range_fit_rejects_no_finite_values_without_mutation() {
        let mut table = sample_table();
        let before = table.clone();
        assert_eq!(
            fit_table_to_values(&mut table, &[f32::NAN, f32::INFINITY, f32::NEG_INFINITY]),
            Err(FitScaleError::NoFiniteValues)
        );
        assert_eq!(table, before);
    }

    #[test]
    fn palette_actions_reuse_the_existing_bound_editable_table() {
        let mut settings = StyleOverrideSettings::default();
        settings.upsert_table(sample_table());
        settings.bind_product("wind_over_15ms", "Custom wind");
        assert_eq!(
            editable_table_name(&settings, "wind_over_15ms", "new colors"),
            "Custom wind"
        );
        assert_eq!(
            editable_table_name(&settings, "unbound_formula", "new colors"),
            "new colors"
        );
    }

    #[test]
    fn fit_is_disabled_when_binding_or_display_units_are_unsafe() {
        let table = sample_table();
        let style = table.to_store_style("Formula wind", "m/s").unwrap();
        let field = FieldData {
            key: FieldKey {
                hour: HourKey {
                    model: "wrf".to_string(),
                    run: "test".to_string(),
                    hour: 0,
                },
                var: "wind_over_15ms".to_string(),
            },
            units: "m/s".to_string(),
            nx: 2,
            ny: 1,
            values: vec![1.0, 20.0],
            range: Some((1.0, 20.0)),
            grid: None,
            lat_descending: false,
            style: Some(style),
        };
        let mut settings = StyleOverrideSettings::default();
        settings.upsert_table(table);
        assert_eq!(
            fit_scale_availability(&settings, Some("Custom wind"), Some(&field)),
            Err(FitScaleUnavailable::PresetNotBound)
        );
        settings.bind_product("wind_over_15ms", "Custom wind");
        assert_eq!(
            fit_scale_availability(&settings, Some("Custom wind"), Some(&field)),
            Ok(())
        );
        let mut mismatched = field;
        mismatched.units = "kt".to_string();
        assert_eq!(
            fit_scale_availability(&settings, Some("Custom wind"), Some(&mismatched)),
            Err(FitScaleUnavailable::UnsafeUnitContext)
        );
    }
}
