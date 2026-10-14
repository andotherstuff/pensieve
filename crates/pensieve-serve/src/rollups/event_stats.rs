//! SQL for `/api/v1/stats/events` and `/api/v1/stats/throughput` over
//! migration 021's `kind_activity` (no new table).
//!
//! Both endpoints used to scan `events_local` (~2.47B rows, no projections):
//! a `created_at` filter reads that column for the whole table, and the
//! `pubkey` of every granule holding a matching row, which for any recent
//! window is every granule (ids are random). Production timings on
//! 2026-10-14: `/stats/events` 5.46 s, `?kind=1` 4.39 s, `?kind=7&days=30`
//! 4.33 s; `/stats/throughput` was only fast while its cached answer lasted.
//!
//! - **`/stats/events`** without `group_by`, or with a `kind` (the paths that
//!   scanned; `group_by` without `kind` keeps reading `daily_user_stats`):
//!   whole UTC days ([`DayWindow`]). Event counts are the sums of the day rows
//!   (exact); `unique_pubkeys` merges their `uniqCombined64` author sketches
//!   across days and, without `kind`, across kinds (approximate, ~0.3%;
//!   sketches merge without loss). Full months inside a long window are read
//!   from month rows ([`DayWindow::month_split`]), so an all-history window
//!   reads at most ~60 day rows and one row per month per kind.
//! - **`/stats/throughput`**: the hour rows from the UTC hour containing
//!   `now() - 7 days`, no upper bound: the same window and number as
//!   `/kinds/{kind}`'s `events_7d`.

use chrono::{DateTime, Datelike, Days, NaiveDate, TimeDelta, Utc};

use super::kind_activity::{KIND_ACTIVITY_TABLE, hour_window};

/// Days of the throughput window (`events_per_hour = total / (7 × 24)`).
pub const THROUGHPUT_DAYS: u32 = 7;

/// Default window of `/stats/events` without `days`, `since` or `until`.
pub const DEFAULT_EVENT_DAYS: u32 = 30;

/// First day a `DateTime` can hold.
fn epoch() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("1970-01-01 exists")
}

/// Whole UTC days `[start, end)` of `created_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayWindow {
    /// First day (inclusive).
    pub start: NaiveDate,
    /// Day after the last day (exclusive); at most tomorrow.
    pub end: NaiveDate,
}

impl DayWindow {
    /// The window of a `/stats/events` request at `now`.
    ///
    /// - `days = N` (wins over `since`/`until`, as before): from the UTC day
    ///   containing `now - N days` through today, i.e. N to N+1 days. This is
    ///   the window the grouped `daily_user_stats` path already used
    ///   (`date >= today() - N AND date <= today()`).
    /// - otherwise `[since, until)`, open ends meaning all history and
    ///   through today.
    ///
    /// Days after today are never read (the old queries required
    /// `created_at <= now()`). Bounds are clamped to `1970-01-01..=tomorrow`,
    /// and an inverted window is empty.
    pub fn new(
        now: DateTime<Utc>,
        days: Option<u32>,
        since: Option<NaiveDate>,
        until: Option<NaiveDate>,
    ) -> Self {
        let today = now.date_naive();
        let tomorrow = today + Days::new(1);
        let (start, until) = match days {
            Some(days) => {
                let start = TimeDelta::try_days(i64::from(days))
                    .and_then(|d| now.checked_sub_signed(d))
                    .map_or(epoch(), |t| t.date_naive());
                (start, None)
            }
            None => (since.unwrap_or(epoch()), until),
        };
        let end = until.map_or(tomorrow, |u| u.min(tomorrow)).max(epoch());
        let start = start.max(epoch()).min(end);
        Self { start, end }
    }

    /// Whether the window holds no day.
    pub fn is_empty(self) -> bool {
        self.start >= self.end
    }

    /// The full months inside the window, `[first, last)`, when there is at
    /// least one: day rows cover `[start, first)` and `[last, end)`.
    pub fn month_split(self) -> Option<(NaiveDate, NaiveDate)> {
        let first = if self.start.day() == 1 {
            self.start
        } else {
            self.start.with_day(1)? + chrono::Months::new(1)
        };
        let last = self.end.with_day(1)?;
        (first < last).then_some((first, last))
    }

    /// Predicate on `kind_activity AS r` selecting the rows that cover the
    /// window exactly once: day rows, or with `months` day rows for the
    /// partial months at either end and month rows for the full months
    /// between.
    pub fn rows_predicate(self, months: bool) -> String {
        let days = |from: NaiveDate, to: NaiveDate| {
            format!(
                "(r.grain = 'day' AND r.period >= {} AND r.period < {})",
                at(from),
                at(to)
            )
        };
        match self.month_split().filter(|_| months) {
            Some((first, last)) => format!(
                "({} OR (r.grain = 'month' AND r.period >= {} AND r.period < {}) OR {})",
                days(self.start, first),
                at(first),
                at(last),
                days(last, self.end)
            ),
            None => days(self.start, self.end),
        }
    }
}

/// Midnight UTC of `day` as a ClickHouse `DateTime` literal.
fn at(day: NaiveDate) -> String {
    format!("toDateTime('{}', 'UTC')", day.format("%Y-%m-%d 00:00:00"))
}

/// `group_by` of `/stats/events`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventGrain {
    /// UTC days.
    Day,
    /// Weeks starting on Monday.
    Week,
    /// UTC months.
    Month,
}

impl EventGrain {
    /// Parse `group_by`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "day" => Some(Self::Day),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            _ => None,
        }
    }

    /// Period of a row (`r.period`), and whether month rows can be read.
    fn period(self) -> (&'static str, bool) {
        match self {
            Self::Day => ("toDate(r.period, 'UTC')", false),
            Self::Week => ("toMonday(toDate(r.period, 'UTC'))", false),
            Self::Month => ("toStartOfMonth(toDate(r.period, 'UTC'))", true),
        }
    }
}

/// `r.kind = ? AND ` when filtering by kind (binds a u16).
fn kind_filter(kind: bool) -> &'static str {
    if kind { "r.kind = ? AND " } else { "" }
}

/// `/stats/events` without `group_by`: `count` (exact) and `unique_pubkeys`
/// (merged author sketches) over `window`, for one kind (binds `kind` as
/// u16) or all kinds.
pub fn events_total_sql(window: DayWindow, kind: bool) -> String {
    format!(
        "SELECT toUInt64(sum(r.event_count)) AS count, \
         uniqCombined64Merge(r.pubkeys) AS unique_pubkeys \
         FROM {KIND_ACTIVITY_TABLE} AS r \
         WHERE {}{}",
        kind_filter(kind),
        window.rows_predicate(true)
    )
}

/// `/stats/events?kind=K&group_by=…`: the `limit` most recent periods of
/// `window` with events, newest first (`period` as `YYYY-MM-DD`, `count`,
/// `unique_pubkeys`). Binds `kind` (u16). A period cut by the window counts
/// only its days inside it, as before.
pub fn events_by_period_sql(window: DayWindow, grain: EventGrain, limit: u32) -> String {
    let (period, months) = grain.period();
    format!(
        "SELECT toString(p) AS period, toUInt64(sum(n)) AS count, \
         uniqCombined64Merge(s) AS unique_pubkeys \
         FROM ( \
         SELECT {period} AS p, r.event_count AS n, r.pubkeys AS s \
         FROM {KIND_ACTIVITY_TABLE} AS r \
         WHERE {}{} \
         ) \
         GROUP BY p \
         ORDER BY p DESC \
         LIMIT {limit}",
        kind_filter(true),
        window.rows_predicate(months)
    )
}

/// `/stats/throughput`: events of the last [`THROUGHPUT_DAYS`] days from the
/// hour rows (from the UTC hour containing `now() - 7 days`, no upper bound),
/// for one kind (binds `kind` as u16) or all kinds.
pub fn throughput_sql(kind: bool) -> String {
    format!(
        "SELECT toUInt64(sum(r.event_count)) \
         FROM {KIND_ACTIVITY_TABLE} AS r \
         WHERE {}{}",
        kind_filter(kind),
        hour_window(THROUGHPUT_DAYS)
    )
}

/// Average events per hour of a [`THROUGHPUT_DAYS`] total.
pub fn events_per_hour(total: u64) -> f64 {
    total as f64 / f64::from(THROUGHPUT_DAYS * 24)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 14, 9, 30, 0).unwrap()
    }

    fn w(start: NaiveDate, end: NaiveDate) -> DayWindow {
        DayWindow { start, end }
    }

    #[test]
    fn rolling_days_become_whole_utc_days_through_today() {
        // 30 days before 09:30 on 2026-10-14 is 2026-09-14 09:30: that whole
        // day through today.
        assert_eq!(
            DayWindow::new(now(), Some(30), None, None),
            w(d(2026, 9, 14), d(2026, 10, 15))
        );
        // days wins over since/until.
        assert_eq!(
            DayWindow::new(now(), Some(1), Some(d(2020, 1, 1)), Some(d(2021, 1, 1))),
            w(d(2026, 10, 13), d(2026, 10, 15))
        );
        // days = 0: today only (as the daily_user_stats path).
        assert_eq!(
            DayWindow::new(now(), Some(0), None, None),
            w(d(2026, 10, 14), d(2026, 10, 15))
        );
        // Beyond 1970 or beyond chrono's range: all history.
        for days in [30_000, u32::MAX] {
            assert_eq!(
                DayWindow::new(now(), Some(days), None, None),
                w(d(1970, 1, 1), d(2026, 10, 15))
            );
        }
    }

    #[test]
    fn since_until_are_clamped_to_history_and_tomorrow() {
        assert_eq!(
            DayWindow::new(now(), None, Some(d(2024, 1, 1)), Some(d(2024, 2, 1))),
            w(d(2024, 1, 1), d(2024, 2, 1))
        );
        assert_eq!(
            DayWindow::new(now(), None, Some(d(2026, 10, 1)), None),
            w(d(2026, 10, 1), d(2026, 10, 15))
        );
        assert_eq!(
            DayWindow::new(now(), None, None, Some(d(2030, 1, 1))),
            w(d(1970, 1, 1), d(2026, 10, 15))
        );
        assert_eq!(
            DayWindow::new(now(), None, Some(d(1900, 1, 1)), Some(d(1960, 1, 1))),
            w(d(1970, 1, 1), d(1970, 1, 1))
        );
        let inverted = DayWindow::new(now(), None, Some(d(2025, 1, 1)), Some(d(2024, 1, 1)));
        assert!(inverted.is_empty());
        assert_eq!(inverted, w(d(2024, 1, 1), d(2024, 1, 1)));
        let future = DayWindow::new(now(), None, Some(d(2200, 1, 1)), None);
        assert!(future.is_empty());
        assert_eq!(future.end, d(2026, 10, 15));
    }

    #[test]
    fn full_months_are_split_out() {
        assert_eq!(w(d(2026, 9, 14), d(2026, 10, 15)).month_split(), None);
        assert_eq!(
            w(d(2026, 9, 1), d(2026, 10, 15)).month_split(),
            Some((d(2026, 9, 1), d(2026, 10, 1)))
        );
        assert_eq!(
            w(d(2024, 1, 1), d(2024, 2, 1)).month_split(),
            Some((d(2024, 1, 1), d(2024, 2, 1)))
        );
        assert_eq!(
            w(d(1970, 1, 1), d(2026, 10, 15)).month_split(),
            Some((d(1970, 1, 1), d(2026, 10, 1)))
        );
        assert_eq!(
            w(d(2025, 12, 31), d(2026, 3, 2)).month_split(),
            Some((d(2026, 1, 1), d(2026, 3, 1)))
        );
        assert_eq!(w(d(2024, 1, 2), d(2024, 2, 1)).month_split(), None);
        assert_eq!(w(d(2024, 1, 1), d(2024, 1, 1)).month_split(), None);
    }

    #[test]
    fn row_predicates_cover_the_window_once() {
        let short = w(d(2026, 9, 14), d(2026, 10, 15));
        let day_only = "(r.grain = 'day' AND r.period >= toDateTime('2026-09-14 00:00:00', 'UTC') \
             AND r.period < toDateTime('2026-10-15 00:00:00', 'UTC'))";
        assert_eq!(short.rows_predicate(true), day_only);
        assert_eq!(short.rows_predicate(false), day_only);
        let long = w(d(2025, 12, 31), d(2026, 3, 2));
        assert_eq!(
            long.rows_predicate(true),
            "((r.grain = 'day' AND r.period >= toDateTime('2025-12-31 00:00:00', 'UTC') \
             AND r.period < toDateTime('2026-01-01 00:00:00', 'UTC')) \
             OR (r.grain = 'month' AND r.period >= toDateTime('2026-01-01 00:00:00', 'UTC') \
             AND r.period < toDateTime('2026-03-01 00:00:00', 'UTC')) \
             OR (r.grain = 'day' AND r.period >= toDateTime('2026-03-01 00:00:00', 'UTC') \
             AND r.period < toDateTime('2026-03-02 00:00:00', 'UTC')))"
        );
        assert!(!long.rows_predicate(false).contains("month"));
    }

    #[test]
    fn event_sql_binds_only_the_kind() {
        let window = w(d(2026, 9, 14), d(2026, 10, 15));
        let all = events_total_sql(window, false);
        assert_eq!(all.matches('?').count(), 0);
        assert!(all.contains("FROM kind_activity AS r WHERE (r.grain = 'day'"));
        let one = events_total_sql(window, true);
        assert_eq!(one.matches('?').count(), 1);
        assert!(one.contains("WHERE r.kind = ? AND ("));
        assert!(one.contains("uniqCombined64Merge(r.pubkeys) AS unique_pubkeys"));

        let week = events_by_period_sql(window, EventGrain::Week, 100);
        assert_eq!(week.matches('?').count(), 1);
        assert!(week.contains("toMonday(toDate(r.period, 'UTC')) AS p"));
        assert!(week.contains("ORDER BY p DESC LIMIT 100"));
        assert!(!week.contains("'month'"));
        let long = w(d(2024, 1, 15), d(2024, 6, 15));
        let month = events_by_period_sql(long, EventGrain::Month, 12);
        assert!(month.contains("toStartOfMonth(toDate(r.period, 'UTC')) AS p"));
        assert!(month.contains("r.grain = 'month'"));
        assert!(!events_by_period_sql(long, EventGrain::Day, 12).contains("'month'"));
        assert_eq!(EventGrain::parse("week"), Some(EventGrain::Week));
        assert_eq!(EventGrain::parse("year"), None);
    }

    #[test]
    fn throughput_reads_seven_days_of_hour_rows() {
        let all = throughput_sql(false);
        assert_eq!(all.matches('?').count(), 0);
        assert!(all.contains(
            "WHERE r.grain = 'hour' AND r.period >= toStartOfHour(now() - toIntervalDay(7), 'UTC')"
        ));
        let one = throughput_sql(true);
        assert_eq!(one.matches('?').count(), 1);
        assert!(one.contains("WHERE r.kind = ? AND r.grain = 'hour'"));
        assert_eq!(events_per_hour(168), 1.0);
        assert_eq!(events_per_hour(0), 0.0);
    }
}
