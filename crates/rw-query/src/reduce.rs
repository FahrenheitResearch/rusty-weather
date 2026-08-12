use rw_store::format::RwsVariableMeta;

use crate::snapshot::{ensure_compatible, ratio};
use crate::{
    MissingPolicy, QueryError, QueryResult, RunSnapshot, ScalarTemporalRequest,
    ScalarTemporalResult,
};

#[derive(Default)]
struct Accumulator {
    minimum: Option<f32>,
    maximum: Option<f32>,
    sum: f64,
    count: u32,
    argmin_time_index: Option<u32>,
    argmax_time_index: Option<u32>,
}

impl Accumulator {
    fn update(&mut self, value: f32, time_index: u32) {
        // Inputs are visited in strictly increasing valid-time order. Strict
        // comparisons intentionally retain the earliest time for ties.
        if self.minimum.is_none_or(|minimum| value < minimum) {
            self.minimum = Some(value);
            self.argmin_time_index = Some(time_index);
        }
        if self.maximum.is_none_or(|maximum| value > maximum) {
            self.maximum = Some(value);
            self.argmax_time_index = Some(time_index);
        }
        self.sum += f64::from(value);
        self.count += 1;
    }
}

pub fn reduce_scalar_temporal(
    snapshot: &RunSnapshot,
    request: &ScalarTemporalRequest,
) -> QueryResult<ScalarTemporalResult> {
    if request.variable.trim().is_empty() {
        return Err(QueryError::InvalidRequest(
            "a scalar variable is required".to_string(),
        ));
    }
    let axis = snapshot.select_timepoints(request.time)?;
    let cells =
        snapshot
            .grid()
            .nx
            .checked_mul(snapshot.grid().ny)
            .ok_or(QueryError::LimitExceeded {
                what: "reduction cells",
                requested: usize::MAX,
                limit: snapshot.limits().max_reduction_cells,
            })?;
    if cells > snapshot.limits().max_reduction_cells {
        return Err(QueryError::LimitExceeded {
            what: "reduction cells",
            requested: cells,
            limit: snapshot.limits().max_reduction_cells,
        });
    }
    let output_values = cells.checked_mul(8).ok_or(QueryError::LimitExceeded {
        what: "scalar reduction output values",
        requested: usize::MAX,
        limit: snapshot.limits().max_point_values,
    })?;
    if output_values > snapshot.limits().max_point_values {
        return Err(QueryError::LimitExceeded {
            what: "scalar reduction output values",
            requested: output_values,
            limit: snapshot.limits().max_point_values,
        });
    }

    let mut accumulators = Vec::new();
    accumulators
        .try_reserve_exact(cells)
        .map_err(|error| QueryError::Allocation {
            what: "scalar accumulators",
            detail: error.to_string(),
        })?;
    accumulators.resize_with(cells, Accumulator::default);

    let mut expected_meta: Option<RwsVariableMeta> = None;
    let mut missing_variable_slots = Vec::new();
    for (time_index, time) in axis.iter().enumerate() {
        let time_index = u32::try_from(time_index).map_err(|_| QueryError::LimitExceeded {
            what: "temporal argument index",
            requested: axis.len(),
            limit: u32::MAX as usize,
        })?;
        let (reader, path) = snapshot.open_reader(time)?;
        let Some(meta) = reader.variable(&request.variable) else {
            if request.missing_policy == MissingPolicy::Strict {
                return Err(QueryError::MissingVariable {
                    variable: request.variable.clone(),
                    slot: time.storage_slot,
                });
            }
            missing_variable_slots.push(time.storage_slot);
            snapshot.ensure_source(&reader, &path, time.storage_slot)?;
            continue;
        };
        if meta.kind != "surface2d" {
            return Err(QueryError::WrongVariableKind {
                variable: request.variable.clone(),
                expected: "surface2d",
                actual: meta.kind.clone(),
            });
        }
        if let Some(expected) = &expected_meta {
            ensure_compatible(expected, meta)?;
        } else {
            expected_meta = Some(meta.clone());
        }

        let values = reader.read_full_2d(&request.variable)?;
        if values.len() != cells {
            return Err(QueryError::InconsistentVariable {
                variable: request.variable.clone(),
                detail: format!("decoded {} cells for {cells}-cell grid", values.len()),
            });
        }
        for (index, (&value, accumulator)) in values.iter().zip(accumulators.iter_mut()).enumerate()
        {
            if value.is_finite() {
                accumulator.update(value, time_index);
            } else if request.missing_policy == MissingPolicy::Strict {
                return Err(QueryError::MissingValue {
                    variable: request.variable.clone(),
                    slot: time.storage_slot,
                    x: index % snapshot.grid().nx,
                    y: index / snapshot.grid().nx,
                });
            }
        }
        snapshot.ensure_source(&reader, &path, time.storage_slot)?;
    }

    let meta =
        expected_meta.ok_or_else(|| QueryError::UnknownVariable(request.variable.clone()))?;
    let mut minimum = reserve_output(cells, "minimum output")?;
    let mut maximum = reserve_output(cells, "maximum output")?;
    let mut range = reserve_output(cells, "range output")?;
    let mut sample_mean = reserve_output(cells, "mean output")?;
    let mut argmin_time_index = reserve_output(cells, "argmin output")?;
    let mut argmax_time_index = reserve_output(cells, "argmax output")?;
    let mut finite_count = reserve_output(cells, "count output")?;
    let mut coverage = reserve_output(cells, "coverage output")?;
    for accumulator in accumulators {
        minimum.push(accumulator.minimum);
        maximum.push(accumulator.maximum);
        range.push(
            accumulator
                .minimum
                .zip(accumulator.maximum)
                .map(|(minimum, maximum)| maximum - minimum),
        );
        sample_mean
            .push((accumulator.count > 0).then(|| accumulator.sum / f64::from(accumulator.count)));
        argmin_time_index.push(accumulator.argmin_time_index);
        argmax_time_index.push(accumulator.argmax_time_index);
        finite_count.push(accumulator.count);
        coverage.push(ratio(accumulator.count as usize, axis.len()));
    }
    snapshot.ensure_manifest_current()?;

    Ok(ScalarTemporalResult {
        run: snapshot.descriptor().clone(),
        variable: request.variable.clone(),
        units: meta.units,
        nx: snapshot.grid().nx,
        ny: snapshot.grid().ny,
        expected_samples: axis.len(),
        axis,
        missing_variable_slots,
        minimum,
        maximum,
        range,
        sample_mean,
        argmin_time_index,
        argmax_time_index,
        finite_count,
        coverage,
    })
}

fn reserve_output<T>(cells: usize, what: &'static str) -> QueryResult<Vec<T>> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(cells)
        .map_err(|error| QueryError::Allocation {
            what,
            detail: error.to_string(),
        })?;
    Ok(output)
}
