#![allow(dead_code)]

//! Percentile-rank math for RTMA-climatology anomaly products.
//!
//! A forecast value is ranked against stored climatology percentile anchors
//! (p05..p99 from the FireWxAtlas ±7-day day-of-year store) by piecewise
//! linear interpolation of the empirical CDF. Values in the unknown tails
//! clamp to the outermost anchor ranks (5 and 99): the store carries no
//! distribution shape beyond them, and fire-weather messaging only needs
//! "at or beyond the Nth percentile" there.

/// Percentile levels of the pack's anchor stats, in CDF order.
pub const ANCHOR_LEVELS: [f32; 8] = [5.0, 10.0, 25.0, 50.0, 75.0, 90.0, 95.0, 99.0];

/// Rank `value` against `anchors` (values at [`ANCHOR_LEVELS`], same order).
/// Returns a rank in `[5, 99]`, or `NaN` when the value or every anchor is
/// non-finite. Quantization can leave tiny non-monotonic wiggles between
/// anchors; they are repaired with a running maximum before interpolation.
/// Flat CDF spans (equal anchor values, e.g. zero joint-hours across most
/// percentiles) rank at the middle of the flat span.
pub fn percentile_rank(value: f32, anchors: &[f32; 8]) -> f32 {
    if !value.is_finite() {
        return f32::NAN;
    }
    // Repair quantization wiggles: enforce non-decreasing anchor values.
    let mut repaired = *anchors;
    let mut any_finite = false;
    let mut running = f32::NEG_INFINITY;
    for anchor in &mut repaired {
        if anchor.is_finite() {
            running = running.max(*anchor);
            *anchor = running;
            any_finite = true;
        } else if any_finite {
            *anchor = running;
        }
    }
    if !any_finite {
        return f32::NAN;
    }
    // Leading non-finite anchors (rare partial-NaN cells): backfill from the
    // first finite value so interpolation sees a full ladder.
    let first_finite = repaired
        .iter()
        .copied()
        .find(|value| value.is_finite())
        .expect("any_finite implies a finite anchor");
    for anchor in &mut repaired {
        if !anchor.is_finite() {
            *anchor = first_finite;
        } else {
            break;
        }
    }

    // Strict tail clamps only: equality falls through to the segment loop so
    // flat CDF spans (equal anchors) can rank at their span midpoint.
    if value < repaired[0] {
        return ANCHOR_LEVELS[0];
    }
    let last = repaired.len() - 1;
    if value > repaired[last] {
        return ANCHOR_LEVELS[last];
    }
    // Find the bracketing segment; flat spans rank at their midpoint.
    for i in 0..last {
        let (lo, hi) = (repaired[i], repaired[i + 1]);
        if value >= lo && value <= hi {
            if hi <= lo {
                // Flat span: extend across all equal-valued anchors.
                let mut j = i + 1;
                while j < last && repaired[j + 1] <= lo {
                    j += 1;
                }
                return 0.5 * (ANCHOR_LEVELS[i] + ANCHOR_LEVELS[j]);
            }
            let t = (value - lo) / (hi - lo);
            return ANCHOR_LEVELS[i] + t * (ANCHOR_LEVELS[i + 1] - ANCHOR_LEVELS[i]);
        }
    }
    ANCHOR_LEVELS[last]
}

/// Rank where LOW values are the dangerous tail (minimum RH): the CDF rank
/// is inverted so "99" always reads as "extreme fire weather for this date".
pub fn dryness_rank(value: f32, anchors: &[f32; 8]) -> f32 {
    let rank = percentile_rank(value, anchors);
    if rank.is_nan() { rank } else { 100.0 - rank + 4.0 } // p05 -> 99, p99 -> 5
}

/// The atlas's no-leap day-of-year: Feb 29 shares Feb 28's climatology
/// (DOY 59), and every date after February maps to its non-leap DOY.
pub fn no_leap_doy(year: i32, month: u32, day: u32) -> u16 {
    const CUM_DAYS: [u16; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let month_index = (month.clamp(1, 12) - 1) as usize;
    let mut day = day.clamp(1, 31) as u16;
    if month == 2 && day == 29 {
        day = 28;
    }
    let _ = year; // leap status intentionally ignored: the store has 365 slots
    CUM_DAYS[month_index] + day
}

/// Civil date of `date_yyyymmdd` + `cycle_utc` + `forecast_hour` (UTC),
/// via Howard Hinnant's days-from-civil round trip — no chrono needed.
pub fn valid_civil_date(
    date_yyyymmdd: &str,
    cycle_utc: u8,
    forecast_hour: u16,
) -> Result<(i32, u32, u32), String> {
    if date_yyyymmdd.len() != 8 || !date_yyyymmdd.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("'{date_yyyymmdd}' is not YYYYMMDD"));
    }
    let year: i32 = date_yyyymmdd[0..4].parse().map_err(|_| "bad year")?;
    let month: u32 = date_yyyymmdd[4..6].parse().map_err(|_| "bad month")?;
    let day: u32 = date_yyyymmdd[6..8].parse().map_err(|_| "bad day")?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(format!("'{date_yyyymmdd}' is out of range"));
    }
    let total_hours = u32::from(cycle_utc) + u32::from(forecast_hour);
    let days = days_from_civil(year, month, day) + i64::from(total_hours / 24);
    Ok(civil_from_days(days))
}

fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((y + i64::from(m <= 2)) as i32, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANCHORS: [f32; 8] = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];

    #[test]
    fn ranks_interpolate_between_anchor_levels() {
        assert_eq!(percentile_rank(4.0, &ANCHORS), 50.0);
        assert_eq!(percentile_rank(4.5, &ANCHORS), 62.5);
        assert_eq!(percentile_rank(6.5, &ANCHORS), 92.5);
        assert!((percentile_rank(2.5, &ANCHORS) - 17.5).abs() < 1e-5);
    }

    #[test]
    fn tails_clamp_to_outermost_anchor_ranks() {
        assert_eq!(percentile_rank(0.0, &ANCHORS), 5.0);
        assert_eq!(percentile_rank(1.0, &ANCHORS), 5.0);
        assert_eq!(percentile_rank(8.0, &ANCHORS), 99.0);
        assert_eq!(percentile_rank(99.0, &ANCHORS), 99.0);
    }

    #[test]
    fn flat_spans_rank_at_their_midpoint() {
        // Joint-hours style cell: zero through p90, then a real tail.
        let anchors = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 2.0, 5.0];
        let rank = percentile_rank(0.0, &anchors);
        assert!((rank - 47.5).abs() < 1e-5, "flat-zero rank {rank}");
        assert!(percentile_rank(1.0, &anchors) > 90.0);
        assert_eq!(percentile_rank(5.0, &anchors), 99.0);
    }

    #[test]
    fn quantization_wiggles_are_repaired() {
        // p25 dips a hair below p10 after i16 round-trip.
        let anchors = [1.0, 2.0, 1.9999, 4.0, 5.0, 6.0, 7.0, 8.0];
        let rank = percentile_rank(2.0, &anchors);
        assert!(rank >= 10.0 && rank <= 25.0, "rank {rank}");
        for probe in [1.5, 2.5, 3.5, 4.5, 7.5] {
            let lower = percentile_rank(probe - 0.25, &anchors);
            let upper = percentile_rank(probe + 0.25, &anchors);
            assert!(upper >= lower, "monotone at {probe}: {lower} vs {upper}");
        }
    }

    #[test]
    fn non_finite_inputs_rank_as_nan() {
        assert!(percentile_rank(f32::NAN, &ANCHORS).is_nan());
        assert!(percentile_rank(3.0, &[f32::NAN; 8]).is_nan());
    }

    #[test]
    fn dryness_rank_inverts_the_scale() {
        // Driest 5% of days (at/below p05 min RH) reads as 99.
        assert_eq!(dryness_rank(1.0, &ANCHORS), 99.0);
        assert_eq!(dryness_rank(8.0, &ANCHORS), 5.0);
        let mid = dryness_rank(4.0, &ANCHORS);
        assert!((mid - 54.0).abs() < 1e-5, "mid {mid}");
    }

    #[test]
    fn valid_dates_roll_across_days_months_and_years() {
        assert_eq!(valid_civil_date("20260629", 3, 3).unwrap(), (2026, 6, 29));
        assert_eq!(valid_civil_date("20260629", 3, 21).unwrap(), (2026, 6, 30));
        assert_eq!(valid_civil_date("20260629", 3, 45).unwrap(), (2026, 7, 1));
        assert_eq!(valid_civil_date("20261231", 23, 1).unwrap(), (2027, 1, 1));
        assert_eq!(valid_civil_date("20240228", 12, 12).unwrap(), (2024, 2, 29));
        assert!(valid_civil_date("2026062", 0, 0).is_err());
    }

    #[test]
    fn no_leap_doy_matches_the_atlas_convention() {
        assert_eq!(no_leap_doy(2026, 1, 1), 1);
        assert_eq!(no_leap_doy(2026, 7, 1), 182);
        assert_eq!(no_leap_doy(2024, 2, 28), 59);
        assert_eq!(no_leap_doy(2024, 2, 29), 59, "Feb 29 shares Feb 28");
        assert_eq!(no_leap_doy(2024, 3, 1), 60, "post-Feb dates use non-leap DOY");
        assert_eq!(no_leap_doy(2026, 12, 31), 365);
    }
}
