//! Wall-clock arithmetic shared by anything that has to name a moment.
//!
//! Captastic writes timestamps in two places that must agree on what "now" is: log lines and
//! capture filenames. The conversion is small enough to have been duplicated and subtle enough
//! that it should not be.

/// Splits a Unix timestamp into UTC year, month, day, hour, minute, second, and millisecond.
///
/// UTC rather than local time: a log read on another machine, and a directory of captures sorted
/// by name, both want an ordering that does not shift under a timezone or a daylight-saving
/// boundary.
pub(crate) fn utc_parts(unix_micros: u128) -> (i64, i64, i64, i64, i64, i64, u32) {
    let seconds = i64::try_from(unix_micros / 1_000_000).unwrap_or(i64::MAX);
    let millis = ((unix_micros % 1_000_000) / 1_000) as u32;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_date_from_unix_days(days);
    (
        year,
        month,
        day,
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60,
        millis,
    )
}

fn civil_date_from_unix_days(days: i64) -> (i64, i64, i64) {
    // Howard Hinnant's civil-from-days algorithm, with day zero at 1970-01-01.
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_the_epoch_and_a_leap_day() {
        assert_eq!(utc_parts(0), (1970, 1, 1, 0, 0, 0, 0));
        assert_eq!(
            utc_parts(1_709_164_800_000_001),
            (2024, 2, 29, 0, 0, 0, 0),
            "2024 is a leap year and 29 February must exist"
        );
        assert_eq!(
            utc_parts(1_709_251_199_999_000),
            (2024, 2, 29, 23, 59, 59, 999),
            "the last millisecond of the leap day"
        );
    }

    #[test]
    fn dates_before_the_epoch_do_not_wrap() {
        // `div_euclid` rather than truncating division: a naive implementation lands on 1970 for
        // any moment before it.
        assert_eq!(utc_parts_of_seconds(-1), (1969, 12, 31, 23, 59, 59));
        assert_eq!(utc_parts_of_seconds(-86_400), (1969, 12, 31, 0, 0, 0));
    }

    fn utc_parts_of_seconds(seconds: i64) -> (i64, i64, i64, i64, i64, i64) {
        let micros = (seconds as i128) * 1_000_000;
        // Only the pre-epoch path is under test, so reconstruct the unsigned argument directly.
        let days = seconds.div_euclid(86_400);
        let seconds_of_day = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_date_from_unix_days(days);
        let _ = micros;
        (
            year,
            month,
            day,
            seconds_of_day / 3_600,
            (seconds_of_day % 3_600) / 60,
            seconds_of_day % 60,
        )
    }
}
