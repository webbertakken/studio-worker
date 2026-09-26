//! How the UI writes durations, ages, days and times.

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};

/// `118 ms`, `12s`, `3m 04s`, `1h 12m`.
pub fn format_duration(d: chrono::Duration) -> String {
    let millis = d.num_milliseconds().max(0);
    if millis < 1000 {
        return format!("{millis} ms");
    }
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        let rem = secs % 60;
        return format!("{mins}m {rem:02}s");
    }
    let hours = mins / 60;
    let rem_min = mins % 60;
    format!("{hours}h {rem_min:02}m")
}

/// `5s ago`, `5m 12s ago`, `2h 05m ago`; a time ahead of `now` is `just now`.
pub fn format_age(now: DateTime<Utc>, when: DateTime<Utc>) -> String {
    let secs = now.signed_duration_since(when).num_seconds();
    if secs < 0 {
        return "just now".into();
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        let rem = secs % 60;
        return format!("{mins}m {rem:02}s ago");
    }
    let hours = mins / 60;
    let rem_min = mins % 60;
    format!("{hours}h {rem_min:02}m ago")
}

/// The heading of a day of jobs: `Today`, `Yesterday`, `Mon 21 Sep`, or
/// `21 Sep 2025` for another year.
pub fn day_label(day: NaiveDate, today: NaiveDate) -> String {
    if day == today {
        "Today".into()
    } else if Some(day) == today.pred_opt() {
        "Yesterday".into()
    } else if day.year() == today.year() {
        day.format("%a %-d %b").to_string()
    } else {
        day.format("%-d %b %Y").to_string()
    }
}

/// `when` as `HH:MM:SS` in `tz`.
pub fn clock<Tz: TimeZone>(when: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    when.with_timezone(tz).format("%H:%M:%S").to_string()
}

/// `when` as `HH:MM:SS` when it is on `now`'s day in `tz`, else as
/// `2026-01-02 03:04`.
pub fn moment<Tz: TimeZone>(when: DateTime<Utc>, now: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    if when.with_timezone(tz).date_naive() == now.with_timezone(tz).date_naive() {
        clock(when, tz)
    } else {
        when.with_timezone(tz).format("%Y-%m-%d %H:%M").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_cover_every_range() {
        let s = chrono::Duration::seconds;
        assert_eq!(
            format_duration(chrono::Duration::milliseconds(118)),
            "118 ms"
        );
        assert_eq!(format_duration(s(12)), "12s");
        assert_eq!(format_duration(s(184)), "3m 04s");
        assert_eq!(format_duration(s(3600 + 12 * 60)), "1h 12m");
        assert_eq!(format_duration(s(-5)), "0 ms");
    }

    #[test]
    fn ages_cover_every_range() {
        let at = |h, m, s| Utc.with_ymd_and_hms(2026, 5, 25, h, m, s).unwrap();
        assert_eq!(format_age(at(12, 0, 30), at(12, 0, 18)), "12s ago");
        assert_eq!(format_age(at(12, 5, 30), at(12, 0, 18)), "5m 12s ago");
        assert_eq!(format_age(at(14, 5, 0), at(12, 0, 0)), "2h 05m ago");
        assert_eq!(format_age(at(12, 0, 0), at(12, 0, 5)), "just now");
    }

    #[test]
    fn days_read_relative_to_today() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        let today = d(2026, 9, 26);
        assert_eq!(day_label(today, today), "Today");
        assert_eq!(day_label(d(2026, 9, 25), today), "Yesterday");
        assert_eq!(day_label(d(2026, 9, 21), today), "Mon 21 Sep");
        assert_eq!(day_label(d(2025, 12, 31), today), "31 Dec 2025");
    }

    #[test]
    fn a_moment_is_a_clock_today_and_a_date_before() {
        let now = Utc.with_ymd_and_hms(2026, 1, 2, 12, 0, 0).unwrap();
        let today = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let before = Utc.with_ymd_and_hms(2026, 1, 1, 23, 4, 5).unwrap();
        assert_eq!(moment(today, now, &Utc), "03:04:05");
        assert_eq!(moment(before, now, &Utc), "2026-01-01 23:04");
    }

    #[test]
    fn times_read_in_the_given_zone() {
        let when = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        assert_eq!(clock(when, &Utc), "03:04:05");
        let plus_two = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(clock(when, &plus_two), "05:04:05");
    }
}
