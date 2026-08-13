use rw_store::format::RwsVariableMeta;
use rw_store::reader::HourReader;

use crate::point::validate_variable_names;
use crate::snapshot::ratio;
use crate::{
    MissingPolicy, PressureProfile, ProfileCycleRequest, ProfileCycleResult, ProfileCycleSample,
    ProfileCycleSampleStatus, ProfileRequest, ProfileResult, QueryError, QueryResult, RunSnapshot,
    SourceProvenance,
};

pub fn query_profile(
    snapshot: &RunSnapshot,
    request: &ProfileRequest,
) -> QueryResult<ProfileResult> {
    validate_variable_names(&request.variables, snapshot.limits().max_variables)?;
    let time = snapshot.timepoint(request.storage_slot)?;
    let (point, fx, fy) = snapshot.locate_fractional(request.latitude, request.longitude)?;
    let (reader, path) = snapshot.open_reader(&time)?;

    let mut profiles = reserved_profiles(request.variables.len())?;
    let mut total_levels = 0usize;
    for name in &request.variables {
        let meta = reader
            .variable(name)
            .ok_or_else(|| QueryError::UnknownVariable(name.clone()))?;
        require_pressure_variable(name, meta)?;
        add_profile_levels(
            &mut total_levels,
            meta.levels_hpa.len(),
            snapshot.limits().max_point_values,
            "profile values",
        )?;
        profiles.push(decode_pressure_profile(
            &reader,
            name,
            meta,
            fx,
            fy,
            &mut || false,
        )?);
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

/// Return pressure profiles for every selected exact stored time.
///
/// An unbounded [`crate::TimeRange`] selects the complete immutable run. The
/// snapshot's selected-time and point-value limits are enforced before each
/// corresponding result allocation. Partial requests retain one sample per
/// selected time and identify absent variables explicitly instead of dropping
/// gaps from the axis.
pub fn query_profile_cycle(
    snapshot: &RunSnapshot,
    request: &ProfileCycleRequest,
) -> QueryResult<ProfileCycleResult> {
    query_profile_cycle_with_cancel(snapshot, request, || false)
}

/// Cancellation-aware form of [`query_profile_cycle`].
///
/// The callback is checked at time and variable boundaries and immediately
/// after each bounded column decode so an HTTP deadline can release its heavy
/// worker cooperatively.
pub fn query_profile_cycle_with_cancel<F>(
    snapshot: &RunSnapshot,
    request: &ProfileCycleRequest,
    mut is_cancelled: F,
) -> QueryResult<ProfileCycleResult>
where
    F: FnMut() -> bool,
{
    validate_variable_names(&request.variables, snapshot.limits().max_variables)?;
    check_cancelled(&mut is_cancelled)?;
    let axis = snapshot.select_timepoints(request.time)?;
    let (point, fx, fy) = snapshot.locate_fractional(request.latitude, request.longitude)?;

    let mut seen = Vec::new();
    seen.try_reserve_exact(request.variables.len())
        .map_err(|error| QueryError::Allocation {
            what: "profile-cycle variable inventory",
            detail: error.to_string(),
        })?;
    seen.resize(request.variables.len(), false);

    let mut samples = Vec::new();
    samples
        .try_reserve_exact(axis.len())
        .map_err(|error| QueryError::Allocation {
            what: "profile-cycle samples",
            detail: error.to_string(),
        })?;
    let mut total_levels = 0usize;

    for time in axis {
        check_cancelled(&mut is_cancelled)?;
        let (reader, path) = snapshot.open_reader(&time)?;
        let mut profiles = reserved_profiles(request.variables.len())?;
        let mut missing_variables = Vec::new();
        missing_variables
            .try_reserve_exact(request.variables.len())
            .map_err(|error| QueryError::Allocation {
                what: "profile-cycle gaps",
                detail: error.to_string(),
            })?;

        for (index, name) in request.variables.iter().enumerate() {
            check_cancelled(&mut is_cancelled)?;
            let Some(meta) = reader.variable(name) else {
                if request.missing_policy == MissingPolicy::Strict {
                    return Err(QueryError::MissingVariable {
                        variable: name.clone(),
                        slot: time.storage_slot,
                    });
                }
                missing_variables.push(name.clone());
                continue;
            };
            seen[index] = true;
            require_pressure_variable(name, meta)?;
            add_profile_levels(
                &mut total_levels,
                meta.levels_hpa.len(),
                snapshot.limits().max_point_values,
                "profile-cycle values",
            )?;
            profiles.push(decode_pressure_profile(
                &reader,
                name,
                meta,
                fx,
                fy,
                &mut is_cancelled,
            )?);
        }

        snapshot.ensure_source(&reader, &path, time.storage_slot)?;
        let source_provenance = snapshot
            .manifest()
            .hours
            .get(&time.storage_slot)
            .ok_or(QueryError::UnknownStorageSlot(time.storage_slot))?
            .source_provenance
            .iter()
            .cloned()
            .map(SourceProvenance::from)
            .collect();
        let status = if missing_variables.is_empty() {
            ProfileCycleSampleStatus::Complete
        } else if profiles.is_empty() {
            ProfileCycleSampleStatus::Gap
        } else {
            ProfileCycleSampleStatus::Partial
        };
        samples.push(ProfileCycleSample {
            time,
            source_provenance,
            status,
            variables: profiles,
            missing_variables,
        });
    }

    if let Some((index, _)) = seen.iter().enumerate().find(|(_, present)| !**present) {
        return Err(QueryError::UnknownVariable(
            request.variables[index].clone(),
        ));
    }
    check_cancelled(&mut is_cancelled)?;
    snapshot.ensure_manifest_current()?;

    Ok(ProfileCycleResult {
        run: snapshot.descriptor().clone(),
        point,
        requested_variables: request.variables.clone(),
        requested_time: request.time,
        missing_policy: request.missing_policy,
        samples,
    })
}

fn reserved_profiles(capacity: usize) -> QueryResult<Vec<PressureProfile>> {
    let mut profiles = Vec::new();
    profiles
        .try_reserve_exact(capacity)
        .map_err(|error| QueryError::Allocation {
            what: "profiles",
            detail: error.to_string(),
        })?;
    Ok(profiles)
}

fn require_pressure_variable(name: &str, meta: &RwsVariableMeta) -> QueryResult<()> {
    if meta.kind != "pressure3d" {
        return Err(QueryError::WrongVariableKind {
            variable: name.to_string(),
            expected: "pressure3d",
            actual: meta.kind.clone(),
        });
    }
    Ok(())
}

fn add_profile_levels(
    total_levels: &mut usize,
    additional_levels: usize,
    limit: usize,
    what: &'static str,
) -> QueryResult<()> {
    *total_levels =
        total_levels
            .checked_add(additional_levels)
            .ok_or(QueryError::LimitExceeded {
                what,
                requested: usize::MAX,
                limit,
            })?;
    if *total_levels > limit {
        return Err(QueryError::LimitExceeded {
            what,
            requested: *total_levels,
            limit,
        });
    }
    Ok(())
}

fn decode_pressure_profile<F>(
    reader: &HourReader,
    name: &str,
    meta: &RwsVariableMeta,
    fx: f64,
    fy: f64,
    is_cancelled: &mut F,
) -> QueryResult<PressureProfile>
where
    F: FnMut() -> bool,
{
    check_cancelled(is_cancelled)?;
    let decoded = reader.read_profile_3d(name, fx, fy)?;
    check_cancelled(is_cancelled)?;
    if decoded.len() != meta.levels_hpa.len() {
        return Err(QueryError::InconsistentVariable {
            variable: name.to_string(),
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
    Ok(PressureProfile {
        name: name.to_string(),
        units: meta.units.clone(),
        levels_hpa: meta.levels_hpa.clone(),
        values,
        available_levels,
        expected_levels,
        coverage: ratio(available_levels, expected_levels),
    })
}

fn check_cancelled<F: FnMut() -> bool>(is_cancelled: &mut F) -> QueryResult<()> {
    if is_cancelled() {
        Err(QueryError::Cancelled)
    } else {
        Ok(())
    }
}
