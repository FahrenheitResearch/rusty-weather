use crate::{QueryError, QueryResult};

/// Parse the only legacy-v1 forecast-run slug accepted by the query layer:
/// `YYYYMMDD_HHz`, interpreted as a UTC run origin.
///
/// Legacy manifests contain no physical timestamps. Refusing arbitrary run
/// names is safer than guessing and publishing a plausible but wrong axis.
pub fn parse_legacy_run_origin_unix(run: &str) -> QueryResult<i64> {
    let bytes = run.as_bytes();
    if bytes.len() != 12 || bytes[8] != b'_' || bytes[11] != b'z' {
        return Err(invalid(run, "expected exactly YYYYMMDD_HHz"));
    }
    for &index in &[0usize, 1, 2, 3, 4, 5, 6, 7, 9, 10] {
        if !bytes[index].is_ascii_digit() {
            return Err(invalid(
                run,
                "date and cycle must contain only ASCII digits",
            ));
        }
    }
    let day_origin = parse_yyyymmdd(run, &run[..8])?;
    let hour = parse_ascii_digits(&bytes[9..11]);
    if !(0..=23).contains(&hour) {
        return Err(invalid(run, "cycle is outside 00..=23"));
    }
    day_origin
        .checked_add(hour * 3_600)
        .ok_or_else(|| invalid(run, "UTC timestamp overflows i64"))
}

/// Parse the date token used by legacy satellite and SimSat stores.
///
/// Those pre-exact-time writers use run names such as
/// `conus_c13_20260822`, `meso1_rgb_g19_20260822`, or the same names with a
/// collision suffix. The frame map key is HHMM and the file is `tHHMM.rws`.
/// This function accepts exactly one real eight-digit date token separated
/// by non-alphanumeric characters; it never guesses from an arbitrary digit
/// substring.
pub fn parse_legacy_observation_day_origin_unix(run: &str) -> QueryResult<i64> {
    let mut dates = run
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| token.len() == 8 && token.bytes().all(|byte| byte.is_ascii_digit()))
        .collect::<Vec<_>>();
    dates.sort_unstable();
    dates.dedup();
    match dates.as_slice() {
        [date] => parse_yyyymmdd(run, date),
        [] => Err(invalid(
            run,
            "expected one separated YYYYMMDD observation date token",
        )),
        _ => Err(invalid(
            run,
            "observation run contains more than one YYYYMMDD date token",
        )),
    }
}

/// Convert a legacy observation manifest key from HHMM to elapsed seconds
/// since UTC midnight. This is intentionally separate from forecast-hour
/// handling so 1847 can never be interpreted as forecast hour 1847.
pub fn parse_legacy_observation_hhmm_slot(run: &str, slot: u16) -> QueryResult<u64> {
    let hour = u64::from(slot / 100);
    let minute = u64::from(slot % 100);
    if hour > 23 || minute > 59 {
        return Err(invalid(
            run,
            format!("observation storage key {slot} is not a valid HHMM time"),
        ));
    }
    Ok(hour * 3_600 + minute * 60)
}

fn parse_yyyymmdd(run: &str, date: &str) -> QueryResult<i64> {
    let bytes = date.as_bytes();
    if bytes.len() != 8 || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(invalid(run, "date must be exactly YYYYMMDD"));
    }
    let year = parse_ascii_digits(&bytes[0..4]);
    let month = parse_ascii_digits(&bytes[4..6]);
    let day = parse_ascii_digits(&bytes[6..8]);
    if !(1..=9_999).contains(&year) {
        return Err(invalid(run, "year must be in 0001..=9999"));
    }
    if !(1..=12).contains(&month) {
        return Err(invalid(run, "month is outside 01..=12"));
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day < 1 || day > month_days[(month - 1) as usize] {
        return Err(invalid(run, "day is invalid for the given month and year"));
    }

    // Howard Hinnant's proleptic-Gregorian days-from-civil algorithm.
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days_since_epoch = era * 146_097 + day_of_era - 719_468;
    days_since_epoch
        .checked_mul(86_400)
        .ok_or_else(|| invalid(run, "UTC timestamp overflows i64"))
}

fn parse_ascii_digits(bytes: &[u8]) -> i64 {
    bytes
        .iter()
        .fold(0, |value, digit| value * 10 + i64::from(digit - b'0'))
}

fn invalid(run: &str, reason: impl Into<String>) -> QueryError {
    QueryError::InvalidLegacyRunSlug {
        run: run.to_string(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_origin_accepts_only_canonical_real_dates() {
        assert_eq!(parse_legacy_run_origin_unix("19700101_00z").unwrap(), 0);
        assert_eq!(
            parse_legacy_run_origin_unix("20240229_06z").unwrap(),
            1_709_186_400
        );
        assert_eq!(
            parse_legacy_run_origin_unix("20000229_23z").unwrap(),
            951_865_200
        );
        for bad in [
            "20230229_00z",
            "20241301_00z",
            "20240100_00z",
            "20240101_24z",
            "20240101_00Z",
            "2024-01-01_00z",
            "20240101_0az",
        ] {
            assert!(parse_legacy_run_origin_unix(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn legacy_observation_runs_extract_one_date_token() {
        let expected = parse_legacy_run_origin_unix("20260822_00z").unwrap();
        assert_eq!(
            parse_legacy_observation_day_origin_unix("conus_c13_20260822").unwrap(),
            expected
        );
        assert_eq!(
            parse_legacy_observation_day_origin_unix("meso1_rgb_g19_20260822_2").unwrap(),
            expected
        );
        assert!(parse_legacy_observation_day_origin_unix("conus_c13").is_err());
        assert!(parse_legacy_observation_day_origin_unix("x_20260821_20260822").is_err());
    }

    #[test]
    fn legacy_observation_hhmm_is_not_a_forecast_hour() {
        assert_eq!(
            parse_legacy_observation_hhmm_slot("conus_c13_20260822", 1847).unwrap(),
            18 * 3_600 + 47 * 60
        );
        for bad in [2360, 2400, 9999] {
            assert!(parse_legacy_observation_hhmm_slot("conus_c13_20260822", bad).is_err());
        }
    }
}
