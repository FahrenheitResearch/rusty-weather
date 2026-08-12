use std::collections::BTreeSet;

use rw_store::format::RwsVariableMeta;

use crate::snapshot::{ensure_compatible, ratio};
use crate::{
    MissingPolicy, PointSeriesRequest, PointSeriesResult, PointVariableSeries, QueryError,
    QueryResult, RunSnapshot,
};

pub fn query_point_series(
    snapshot: &RunSnapshot,
    request: &PointSeriesRequest,
) -> QueryResult<PointSeriesResult> {
    validate_variable_names(&request.variables, snapshot.limits().max_variables)?;
    let axis = snapshot.select_timepoints(request.time)?;
    let value_count =
        axis.len()
            .checked_mul(request.variables.len())
            .ok_or(QueryError::LimitExceeded {
                what: "point values",
                requested: usize::MAX,
                limit: snapshot.limits().max_point_values,
            })?;
    if value_count > snapshot.limits().max_point_values {
        return Err(QueryError::LimitExceeded {
            what: "point values",
            requested: value_count,
            limit: snapshot.limits().max_point_values,
        });
    }
    let point = snapshot.locate_point(request.latitude, request.longitude)?;

    struct Building {
        name: String,
        meta: Option<RwsVariableMeta>,
        values: Vec<Option<f32>>,
    }
    let mut building = Vec::new();
    building
        .try_reserve_exact(request.variables.len())
        .map_err(|error| QueryError::Allocation {
            what: "point variables",
            detail: error.to_string(),
        })?;
    for name in &request.variables {
        let mut values = Vec::new();
        values
            .try_reserve_exact(axis.len())
            .map_err(|error| QueryError::Allocation {
                what: "point series",
                detail: error.to_string(),
            })?;
        building.push(Building {
            name: name.clone(),
            meta: None,
            values,
        });
    }

    for time in &axis {
        let (reader, path) = snapshot.open_reader(time)?;
        for variable in &mut building {
            let Some(meta) = reader.variable(&variable.name) else {
                if request.missing_policy == MissingPolicy::Strict {
                    return Err(QueryError::MissingVariable {
                        variable: variable.name.clone(),
                        slot: time.storage_slot,
                    });
                }
                variable.values.push(None);
                continue;
            };
            if meta.kind != "surface2d" {
                return Err(QueryError::WrongVariableKind {
                    variable: variable.name.clone(),
                    expected: "surface2d",
                    actual: meta.kind.clone(),
                });
            }
            if let Some(expected) = &variable.meta {
                ensure_compatible(expected, meta)?;
            } else {
                variable.meta = Some(meta.clone());
            }
            let value = reader.read_point_2d(&variable.name, point.x, point.y)?;
            if value.is_finite() {
                variable.values.push(Some(value));
            } else if request.missing_policy == MissingPolicy::Strict {
                return Err(QueryError::MissingValue {
                    variable: variable.name.clone(),
                    slot: time.storage_slot,
                    x: point.x,
                    y: point.y,
                });
            } else {
                variable.values.push(None);
            }
        }
        snapshot.ensure_source(&reader, &path, time.storage_slot)?;
    }

    let expected_samples = axis.len();
    let variables = building
        .into_iter()
        .map(|variable| {
            let meta = variable
                .meta
                .ok_or_else(|| QueryError::UnknownVariable(variable.name.clone()))?;
            let available_samples = variable.values.iter().flatten().count();
            Ok(PointVariableSeries {
                name: variable.name,
                units: meta.units,
                values: variable.values,
                available_samples,
                expected_samples,
                coverage: ratio(available_samples, expected_samples),
            })
        })
        .collect::<QueryResult<Vec<_>>>()?;
    snapshot.ensure_manifest_current()?;

    Ok(PointSeriesResult {
        run: snapshot.descriptor().clone(),
        point,
        axis,
        variables,
    })
}

pub(crate) fn validate_variable_names(names: &[String], limit: usize) -> QueryResult<()> {
    if names.is_empty() {
        return Err(QueryError::InvalidRequest(
            "at least one variable is required".to_string(),
        ));
    }
    if names.len() > limit {
        return Err(QueryError::LimitExceeded {
            what: "variables",
            requested: names.len(),
            limit,
        });
    }
    let mut unique = BTreeSet::new();
    for name in names {
        if name.trim().is_empty() {
            return Err(QueryError::InvalidRequest(
                "variable names must not be empty".to_string(),
            ));
        }
        if !unique.insert(name) {
            return Err(QueryError::InvalidRequest(format!(
                "duplicate variable '{name}'"
            )));
        }
    }
    Ok(())
}
