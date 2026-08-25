use std::collections::{BTreeMap, BTreeSet};

use rw_ops_protocol::{
    GeoPoint, ModelInputSource, STORM_CELL_FRAME_SCHEMA, StormCell, StormCellFrame,
    StormMethodIdentity, StormMethodKind, StormModelBackend, StormModelInput, StormSource,
};
use sha2::{Digest, Sha256};

use crate::{
    DistributionAudience, InstalledModel, ModelKey, ModelLimits, ModelRegistry, RegistryError,
    RegistryResult,
};

#[derive(Clone, Copy, Debug)]
pub enum GridGeometry<'a> {
    Geographic {
        longitudes: &'a [f64],
        latitudes: &'a [f64],
    },
    Level2Cartesian {
        east_m: &'a [f64],
        north_m: &'a [f64],
        radar_location: GeoPoint,
    },
}

impl<'a> GridGeometry<'a> {
    pub fn shape(self, limits: ModelLimits) -> RegistryResult<(usize, usize)> {
        let (x, y) = match self {
            Self::Geographic {
                longitudes,
                latitudes,
            } => (longitudes, latitudes),
            Self::Level2Cartesian {
                east_m,
                north_m,
                radar_location,
            } => {
                radar_location.validate()?;
                (east_m, north_m)
            }
        };
        if x.len() < 2 || y.len() < 2 {
            return Err(RegistryError::IncompatibleInput(
                "grid axes each require at least two coordinates".into(),
            ));
        }
        if x.len() > limits.max_grid_width || y.len() > limits.max_grid_height {
            return Err(RegistryError::IncompatibleInput(format!(
                "grid {}x{} exceeds configured dimensions {}x{}",
                x.len(),
                y.len(),
                limits.max_grid_width,
                limits.max_grid_height
            )));
        }
        let points = x
            .len()
            .checked_mul(y.len())
            .ok_or_else(|| RegistryError::IncompatibleInput("grid size overflow".into()))?;
        if points > limits.max_grid_points {
            return Err(RegistryError::IncompatibleInput(format!(
                "grid has {points} points, configured maximum is {}",
                limits.max_grid_points
            )));
        }
        validate_axis("x", x)?;
        validate_axis("y", y)?;
        if matches!(self, Self::Geographic { .. }) {
            if let Some((index, value)) = x
                .iter()
                .copied()
                .enumerate()
                .find(|(_, value)| !(-180.0..=180.0).contains(value))
            {
                return Err(RegistryError::IncompatibleInput(format!(
                    "longitude {value} at index {index} is outside [-180, 180]"
                )));
            }
            if let Some((index, value)) = y
                .iter()
                .copied()
                .enumerate()
                .find(|(_, value)| !(-90.0..=90.0).contains(value))
            {
                return Err(RegistryError::IncompatibleInput(format!(
                    "latitude {value} at index {index} is outside [-90, 90]"
                )));
            }
        }
        Ok((x.len(), y.len()))
    }

    fn window(self, x_start: usize, x_end: usize, y_start: usize, y_end: usize) -> Self {
        match self {
            Self::Geographic {
                longitudes,
                latitudes,
            } => Self::Geographic {
                longitudes: &longitudes[x_start..x_end],
                latitudes: &latitudes[y_start..y_end],
            },
            Self::Level2Cartesian {
                east_m,
                north_m,
                radar_location,
            } => Self::Level2Cartesian {
                east_m: &east_m[x_start..x_end],
                north_m: &north_m[y_start..y_end],
                radar_location,
            },
        }
    }
}

fn validate_axis(name: &'static str, axis: &[f64]) -> RegistryResult<()> {
    if let Some(index) = axis.iter().position(|value| !value.is_finite()) {
        return Err(RegistryError::IncompatibleInput(format!(
            "{name} axis value at index {index} is non-finite"
        )));
    }
    let ascending = axis[1] > axis[0];
    if axis[1] == axis[0]
        || axis
            .windows(2)
            .any(|pair| pair[0] == pair[1] || (pair[1] > pair[0]) != ascending)
    {
        return Err(RegistryError::IncompatibleInput(format!(
            "{name} axis must be strictly monotonic"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct ModelInputPlane<'a> {
    pub name: &'a str,
    pub source: ModelInputSource,
    pub field: &'a str,
    pub units: &'a str,
    pub values: &'a [f32],
}

#[derive(Clone, Copy, Debug)]
pub struct ModelInputBatch<'a> {
    pub source: &'a StormSource,
    pub geometry: GridGeometry<'a>,
    pub planes: &'a [ModelInputPlane<'a>],
}

pub fn validate_model_inputs(
    model: &InstalledModel,
    batch: ModelInputBatch<'_>,
    limits: ModelLimits,
) -> RegistryResult<()> {
    batch.source.validate()?;
    let (width, height) = batch.geometry.shape(limits)?;
    let expected_points = width
        .checked_mul(height)
        .ok_or_else(|| RegistryError::IncompatibleInput("grid size overflow".into()))?;
    if batch.planes.len() != model.manifest.inputs.len()
        || batch.planes.len() > limits.max_input_planes
    {
        return Err(RegistryError::IncompatibleInput(format!(
            "received {} input planes; model requires {} and configured maximum is {}",
            batch.planes.len(),
            model.manifest.inputs.len(),
            limits.max_input_planes
        )));
    }

    let mut actual_by_name = BTreeMap::new();
    for plane in batch.planes {
        if actual_by_name.insert(plane.name, plane).is_some() {
            return Err(RegistryError::IncompatibleInput(format!(
                "duplicate input plane '{}'",
                plane.name
            )));
        }
        if plane.values.len() != expected_points {
            return Err(RegistryError::IncompatibleInput(format!(
                "input '{}' has {} values, expected {expected_points}",
                plane.name,
                plane.values.len()
            )));
        }
    }

    for expected in &model.manifest.inputs {
        let actual = actual_by_name.get(expected.name.as_str()).ok_or_else(|| {
            RegistryError::IncompatibleInput(format!("missing input '{}'", expected.name))
        })?;
        if actual.source != expected.source
            || actual.field != expected.field
            || actual.units != expected.units
        {
            return Err(RegistryError::IncompatibleInput(format!(
                "input '{}' descriptor differs: expected {:?}/'{}/{}', received {:?}/'{}/{}'",
                expected.name,
                expected.source,
                expected.field,
                expected.units,
                actual.source,
                actual.field,
                actual.units
            )));
        }
        validate_input_source(batch.source, expected)?;
    }
    Ok(())
}

fn validate_input_source(source: &StormSource, input: &StormModelInput) -> RegistryResult<()> {
    let compatible = matches!(input.source, ModelInputSource::DerivedField)
        || matches!(
            (&input.source, source),
            (ModelInputSource::MrmsProduct, StormSource::Mrms { .. })
                | (
                    ModelInputSource::NexradMoment,
                    StormSource::NexradLevel2 { .. }
                )
        );
    if compatible {
        Ok(())
    } else {
        Err(RegistryError::IncompatibleInput(format!(
            "input '{}' source kind is incompatible with the requested radar source",
            input.name
        )))
    }
}

#[derive(Clone, Copy, Debug)]
pub enum MaskOutput<'a> {
    Probabilities {
        width: usize,
        height: usize,
        values: &'a [f32],
    },
    Labels {
        width: usize,
        height: usize,
        values: &'a [u32],
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum OwnedMask {
    Probabilities {
        width: usize,
        height: usize,
        values: Vec<f32>,
    },
    Labels {
        width: usize,
        height: usize,
        values: Vec<u32>,
    },
}

impl OwnedMask {
    pub fn as_output(&self) -> MaskOutput<'_> {
        match self {
            Self::Probabilities {
                width,
                height,
                values,
            } => MaskOutput::Probabilities {
                width: *width,
                height: *height,
                values,
            },
            Self::Labels {
                width,
                height,
                values,
            } => MaskOutput::Labels {
                width: *width,
                height: *height,
                values,
            },
        }
    }
}

pub fn canonicalize_supplied_mask(
    registry: &ModelRegistry,
    key: &ModelKey,
    source: StormSource,
    generated_at_unix_ms: i64,
    geometry: GridGeometry<'_>,
    output: MaskOutput<'_>,
    audience: DistributionAudience,
) -> RegistryResult<StormCellFrame> {
    let model = registry.enabled_for_execution(key)?;
    if model.manifest.backend != StormModelBackend::SuppliedMask {
        return Err(RegistryError::BackendUnavailable(
            "requested model is not a supplied-mask backend",
        ));
    }
    canonicalize_model_mask(
        model,
        source,
        generated_at_unix_ms,
        geometry,
        output,
        audience,
        registry.limits(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn canonicalize_model_mask(
    model: &InstalledModel,
    source: StormSource,
    generated_at_unix_ms: i64,
    geometry: GridGeometry<'_>,
    output: MaskOutput<'_>,
    audience: DistributionAudience,
    limits: ModelLimits,
) -> RegistryResult<StormCellFrame> {
    source.validate()?;
    if generated_at_unix_ms <= 0 {
        return Err(RegistryError::InvalidOutput(
            "generated timestamp must be positive".into(),
        ));
    }
    model.authorize_derived_output(audience)?;
    let shape = geometry.shape(limits)?;
    match output {
        MaskOutput::Probabilities {
            width,
            height,
            values,
        } => canonicalize_probabilities(
            model,
            source,
            generated_at_unix_ms,
            geometry,
            shape,
            width,
            height,
            values,
            limits,
        ),
        MaskOutput::Labels {
            width,
            height,
            values,
        } => canonicalize_labels(
            model,
            source,
            generated_at_unix_ms,
            geometry,
            shape,
            width,
            height,
            values,
            limits,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn canonicalize_probabilities(
    model: &InstalledModel,
    source: StormSource,
    generated_at_unix_ms: i64,
    geometry: GridGeometry<'_>,
    expected_shape: (usize, usize),
    width: usize,
    height: usize,
    values: &[f32],
    _limits: ModelLimits,
) -> RegistryResult<StormCellFrame> {
    validate_mask_shape(expected_shape, width, height, values.len())?;
    if let Some((index, value)) = values
        .iter()
        .copied()
        .enumerate()
        .find(|(_, value)| value.is_finite() && !(0.0..=1.0).contains(value))
    {
        return Err(RegistryError::InvalidOutput(format!(
            "probability {value} at index {index} is outside [0, 1]"
        )));
    }
    let mut frame = detect_binary(
        source,
        generated_at_unix_ms,
        geometry,
        values,
        model.manifest.probability_threshold as f32,
        model.manifest.minimum_area_km2.unwrap_or(0.0),
    )?;
    for cell in &mut frame.cells {
        cell.confidence = cell.maximum_reflectivity_dbz;
        cell.maximum_reflectivity_dbz = None;
        cell.attributes
            .insert("mask_kind".into(), "probability".into());
        cell.attributes.insert(
            "geometry_provenance".into(),
            "model_probability_threshold_contour".into(),
        );
        reidentify_cell(model, cell, None);
    }
    finalize_frame(model, frame, "probability", width, height)
}

#[allow(clippy::too_many_arguments)]
fn canonicalize_labels(
    model: &InstalledModel,
    source: StormSource,
    generated_at_unix_ms: i64,
    geometry: GridGeometry<'_>,
    expected_shape: (usize, usize),
    width: usize,
    height: usize,
    labels: &[u32],
    limits: ModelLimits,
) -> RegistryResult<StormCellFrame> {
    validate_mask_shape(expected_shape, width, height, labels.len())?;
    let mut bounds = BTreeMap::<u32, Bounds>::new();
    for (index, label) in labels.iter().copied().enumerate() {
        if label == 0 {
            continue;
        }
        let x = index % width;
        let y = index / width;
        bounds
            .entry(label)
            .and_modify(|bounds| bounds.include(x, y))
            .or_insert_with(|| Bounds::new(x, y));
    }

    let mut work_points = 0_usize;
    let mut cells = Vec::new();
    let mut warnings = BTreeSet::new();
    for (label, bounds) in bounds {
        let x_start = bounds.min_x.saturating_sub(1);
        let y_start = bounds.min_y.saturating_sub(1);
        let x_end = (bounds.max_x + 2).min(width);
        let y_end = (bounds.max_y + 2).min(height);
        let window_width = x_end - x_start;
        let window_height = y_end - y_start;
        let window_points = window_width
            .checked_mul(window_height)
            .ok_or_else(|| RegistryError::InvalidOutput("label window size overflow".into()))?;
        work_points = work_points.checked_add(window_points).ok_or_else(|| {
            RegistryError::InvalidOutput("aggregate label work size overflow".into())
        })?;
        if work_points > limits.max_label_work_points {
            return Err(RegistryError::InvalidOutput(format!(
                "aggregate label contour work has {work_points} points; configured maximum is {}",
                limits.max_label_work_points
            )));
        }

        let mut binary = Vec::new();
        binary.try_reserve_exact(window_points).map_err(|_| {
            RegistryError::InvalidOutput("could not allocate label contour window".into())
        })?;
        for y in y_start..y_end {
            let row = &labels[y * width + x_start..y * width + x_end];
            binary.extend(row.iter().map(|value| u8::from(*value == label) as f32));
        }
        let mut frame = detect_binary(
            source.clone(),
            generated_at_unix_ms,
            geometry.window(x_start, x_end, y_start, y_end),
            &binary,
            0.5,
            model.manifest.minimum_area_km2.unwrap_or(0.0),
        )?;
        for warning in frame.warnings {
            warnings.insert(warning);
        }
        for mut cell in frame.cells.drain(..) {
            cell.maximum_reflectivity_dbz = None;
            cell.confidence = None;
            cell.attributes
                .insert("mask_kind".into(), "integer_label".into());
            cell.attributes
                .insert("supplied_label".into(), label.to_string());
            cell.attributes.insert(
                "geometry_provenance".into(),
                "model_integer_label_cell_boundary".into(),
            );
            reidentify_cell(model, &mut cell, Some(label));
            cells.push(cell);
        }
    }
    cells.sort_by(|left, right| left.cell_id.cmp(&right.cell_id));
    let frame = StormCellFrame {
        schema: STORM_CELL_FRAME_SCHEMA.into(),
        generated_at_unix_ms,
        source,
        method: model_method(model, "integer_label", width, height),
        cells,
        partial: false,
        warnings: warnings.into_iter().collect(),
    };
    frame.validate()?;
    Ok(frame)
}

fn detect_binary(
    source: StormSource,
    generated_at_unix_ms: i64,
    geometry: GridGeometry<'_>,
    values: &[f32],
    threshold: f32,
    minimum_area_km2: f64,
) -> RegistryResult<StormCellFrame> {
    let config = rw_storm::DetectionConfig {
        threshold_dbz: threshold,
        minimum_valid_dbz: 0.0,
        maximum_valid_dbz: 1.0,
        minimum_gate_count: 1,
        minimum_area_km2,
        connectivity: rw_storm::Connectivity::Four,
    };
    match geometry {
        GridGeometry::Geographic {
            longitudes,
            latitudes,
        } => Ok(rw_storm::detect_geographic(
            source,
            generated_at_unix_ms,
            rw_storm::GeographicGrid {
                values_dbz: values,
                longitudes,
                latitudes,
            },
            config,
        )?),
        GridGeometry::Level2Cartesian {
            east_m,
            north_m,
            radar_location,
        } => Ok(rw_storm::detect_level2_cartesian(
            source,
            generated_at_unix_ms,
            rw_storm::Level2CartesianGrid {
                values_dbz: values,
                east_m,
                north_m,
                radar_location,
            },
            config,
        )?),
    }
}

fn validate_mask_shape(
    expected: (usize, usize),
    width: usize,
    height: usize,
    actual_values: usize,
) -> RegistryResult<()> {
    if (width, height) != expected {
        return Err(RegistryError::InvalidOutput(format!(
            "mask dimensions {width}x{height} do not match grid {}x{}",
            expected.0, expected.1
        )));
    }
    let expected_values = width
        .checked_mul(height)
        .ok_or_else(|| RegistryError::InvalidOutput("mask size overflow".into()))?;
    if actual_values != expected_values {
        return Err(RegistryError::InvalidOutput(format!(
            "mask has {actual_values} values, expected {expected_values}"
        )));
    }
    Ok(())
}

fn finalize_frame(
    model: &InstalledModel,
    mut frame: StormCellFrame,
    mask_kind: &'static str,
    width: usize,
    height: usize,
) -> RegistryResult<StormCellFrame> {
    frame.method = model_method(model, mask_kind, width, height);
    frame
        .cells
        .sort_by(|left, right| left.cell_id.cmp(&right.cell_id));
    frame.validate()?;
    Ok(frame)
}

fn model_method(
    model: &InstalledModel,
    mask_kind: &'static str,
    width: usize,
    height: usize,
) -> StormMethodIdentity {
    let mut parameters = BTreeMap::new();
    parameters.insert(
        "artifact_sha256".into(),
        model.manifest.artifact_sha256.clone(),
    );
    parameters.insert(
        "backend".into(),
        backend_name(model.manifest.backend).into(),
    );
    parameters.insert("mask_kind".into(), mask_kind.into());
    parameters.insert(
        "probability_threshold".into(),
        model.manifest.probability_threshold.to_string(),
    );
    parameters.insert(
        "minimum_area_km2".into(),
        model.manifest.minimum_area_km2.unwrap_or(0.0).to_string(),
    );
    parameters.insert("grid_width".into(), width.to_string());
    parameters.insert("grid_height".into(), height.to_string());
    parameters.insert(
        "contour_engine".into(),
        "rw-storm_weather-contours_oirt".into(),
    );
    StormMethodIdentity {
        method_id: "rw-storm-ml".into(),
        method_version: model.manifest.model_version.clone(),
        kind: StormMethodKind::MachineLearning,
        display_name: model.manifest.display_name.clone(),
        description: model.manifest.description.clone(),
        upstream_product: None,
        model_id: Some(model.manifest.model_id.clone()),
        model_version: Some(model.manifest.model_version.clone()),
        parameters,
    }
}

fn backend_name(backend: StormModelBackend) -> &'static str {
    match backend {
        StormModelBackend::NativeRust => "native_rust",
        StormModelBackend::TractOnnx => "tract_onnx",
        StormModelBackend::SuppliedMask => "supplied_mask",
    }
}

fn reidentify_cell(model: &InstalledModel, cell: &mut StormCell, label: Option<u32>) {
    let mut digest = Sha256::new();
    digest.update(b"rw-storm-ml-cell-v1\0");
    digest.update(model.manifest.model_id.as_bytes());
    digest.update(b"\0");
    digest.update(model.manifest.model_version.as_bytes());
    digest.update(b"\0");
    digest.update(model.manifest.artifact_sha256.as_bytes());
    digest.update(b"\0");
    digest.update(cell.cell_id.as_bytes());
    if let Some(label) = label {
        digest.update(label.to_le_bytes());
    }
    let bytes = digest.finalize();
    cell.cell_id = format!("mlcell-{}", short_hex(&bytes));
}

fn short_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(40);
    for byte in &bytes[..20] {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[derive(Clone, Copy, Debug)]
struct Bounds {
    min_x: usize,
    max_x: usize,
    min_y: usize,
    max_y: usize,
}

impl Bounds {
    fn new(x: usize, y: usize) -> Self {
        Self {
            min_x: x,
            max_x: x,
            min_y: y,
            max_y: y,
        }
    }

    fn include(&mut self, x: usize, y: usize) {
        self.min_x = self.min_x.min(x);
        self.max_x = self.max_x.max(x);
        self.min_y = self.min_y.min(y);
        self.max_y = self.max_y.max(y);
    }
}
