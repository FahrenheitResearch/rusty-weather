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
            if ui
                .add_enabled(can_create, egui::Button::new("Apply to map"))
                .clicked()
            {
                if let Some(template) = self.selected_template().cloned() {
                    self.create_table_from_template(&template, Some(&product));
                }
            }
            if ui
                .add_enabled(can_create, egui::Button::new("Save copy"))
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
                .add_enabled(can_apply, egui::Button::new("Apply to map"))
                .clicked()
            {
                if let (Some(field), Some(table)) = (current_field, self.selected_table.clone()) {
                    let product = normalize_product_key(&field.key.var);
                    self.settings.bind_product(&product, &table);
                    self.status = Some(format!("Applied {table} to {product}"));
                    self.changed = true;
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
