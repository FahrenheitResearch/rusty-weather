use crate::point::validate_variable_names;
use crate::snapshot::ratio;
use crate::{PressureProfile, ProfileRequest, ProfileResult, QueryError, QueryResult, RunSnapshot};

pub fn query_profile(
    snapshot: &RunSnapshot,
    request: &ProfileRequest,
) -> QueryResult<ProfileResult> {
    validate_variable_names(&request.variables, snapshot.limits().max_variables)?;
    let time = snapshot.timepoint(request.storage_slot)?;
    let (point, fx, fy) = snapshot.locate_fractional(request.latitude, request.longitude)?;
    let (reader, path) = snapshot.open_reader(&time)?;

    let mut profiles = Vec::new();
    profiles
        .try_reserve_exact(request.variables.len())
        .map_err(|error| QueryError::Allocation {
            what: "profiles",
            detail: error.to_string(),
        })?;
    let mut total_levels = 0usize;
    for name in &request.variables {
        let meta = reader
            .variable(name)
            .ok_or_else(|| QueryError::UnknownVariable(name.clone()))?;
        if meta.kind != "pressure3d" {
            return Err(QueryError::WrongVariableKind {
                variable: name.clone(),
                expected: "pressure3d",
                actual: meta.kind.clone(),
            });
        }
        total_levels =
            total_levels
                .checked_add(meta.levels_hpa.len())
                .ok_or(QueryError::LimitExceeded {
                    what: "profile values",
                    requested: usize::MAX,
                    limit: snapshot.limits().max_point_values,
                })?;
        if total_levels > snapshot.limits().max_point_values {
            return Err(QueryError::LimitExceeded {
                what: "profile values",
                requested: total_levels,
                limit: snapshot.limits().max_point_values,
            });
        }
        let decoded = reader.read_profile_3d(name, fx, fy)?;
        if decoded.len() != meta.levels_hpa.len() {
            return Err(QueryError::InconsistentVariable {
                variable: name.clone(),
                detail: format!(
                    "decoded {} levels for {} metadata levels",
                    decoded.len(),
                    meta.levels_hpa.len()
                ),
            });
        }
        let values: Vec<_> = decoded
            .into_iter()
            .map(|value| value.is_finite().then_some(value))
            .collect();
        let available_levels = values.iter().flatten().count();
        let expected_levels = values.len();
        profiles.push(PressureProfile {
            name: name.clone(),
            units: meta.units.clone(),
            levels_hpa: meta.levels_hpa.clone(),
            values,
            available_levels,
            expected_levels,
            coverage: ratio(available_levels, expected_levels),
        });
    }
    snapshot.ensure_source(&reader, &path, time.storage_slot)?;
    snapshot.ensure_manifest_current()?;

    Ok(ProfileResult {
        run: snapshot.descriptor().clone(),
        point,
        time,
        variables: profiles,
    })
}
