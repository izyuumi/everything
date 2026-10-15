use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, LocalResult, NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::{GapInfo, Tz};
use croner::Cron;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::{MAX_TIMESTAMP, ScheduleError, ScheduleSpec, parse_cron, parse_time_zone, timestamp};
use crate::protocol::UnixMillis;

const MAX_CANDIDATES: usize = 100_000;
const MAX_CALCULATION_TIME: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ClockError {
    #[error("schedule calculation cancelled")]
    Cancelled,
    #[error("invalid schedule calculation: {0}")]
    Invalid(&'static str),
    #[error("schedule calculation blocked: {0}")]
    Blocked(&'static str),
}

pub fn next_after(
    spec: &ScheduleSpec,
    after: UnixMillis,
    cancellation: &CancellationToken,
) -> Result<Option<UnixMillis>, ClockError> {
    check_cancelled(cancellation)?;
    validate(spec, after)?;
    match spec {
        ScheduleSpec::Once { at } => Ok((*at > after).then_some(*at)),
        ScheduleSpec::Interval { anchor, every_ms } => {
            if after < *anchor {
                return Ok(Some(*anchor));
            }
            let steps = (after.get() - anchor.get())
                .checked_div(*every_ms)
                .and_then(|steps| steps.checked_add(1))
                .ok_or(ClockError::Invalid("interval arithmetic overflow"))?;
            let next = steps
                .checked_mul(*every_ms)
                .and_then(|elapsed| anchor.get().checked_add(elapsed))
                .ok_or(ClockError::Invalid("interval arithmetic overflow"))?;
            Ok(Some(checked_millis(next)?))
        }
        ScheduleSpec::Cron {
            expression,
            time_zone,
        } => {
            let calendar = Calendar::new(expression, time_zone)?;
            let boundary = utc(after)?;
            let cursor = integral_second(boundary.with_timezone(&calendar.zone).naive_local())?;
            calendar.search(cursor, boundary, None, Direction::Forward, cancellation)
        }
    }
}

pub fn latest_at_or_before(
    spec: &ScheduleSpec,
    lower_bound: UnixMillis,
    at: UnixMillis,
    cancellation: &CancellationToken,
) -> Result<Option<UnixMillis>, ClockError> {
    check_cancelled(cancellation)?;
    validate(spec, at)?;
    timestamp(lower_bound).map_err(invalid_schedule)?;
    if lower_bound > at {
        return Err(ClockError::Invalid("lower bound exceeds upper bound"));
    }
    match spec {
        ScheduleSpec::Once { at: scheduled } => {
            Ok((*scheduled >= lower_bound && *scheduled <= at).then_some(*scheduled))
        }
        ScheduleSpec::Interval { anchor, every_ms } => {
            if at < *anchor {
                return Ok(None);
            }
            let elapsed = ((at.get() - anchor.get()) / every_ms)
                .checked_mul(*every_ms)
                .and_then(|elapsed| anchor.get().checked_add(elapsed))
                .ok_or(ClockError::Invalid("interval arithmetic overflow"))?;
            let latest = checked_millis(elapsed)?;
            Ok((latest >= lower_bound).then_some(latest))
        }
        ScheduleSpec::Cron {
            expression,
            time_zone,
        } => {
            let calendar = Calendar::new(expression, time_zone)?;
            let boundary = utc(at)?;
            let local = boundary.with_timezone(&calendar.zone);
            let mut cursor = local.naive_local();
            if let LocalResult::Ambiguous(first, second) =
                calendar.zone.from_local_datetime(&cursor)
            {
                let earlier = first.min(second);
                let later = first.max(second);
                if local == later {
                    cursor = cursor
                        .checked_add_signed(later.signed_duration_since(earlier))
                        .ok_or(ClockError::Invalid("fold boundary overflow"))?;
                }
            }
            calendar.search(
                integral_second(cursor)?,
                boundary,
                Some(utc(lower_bound)?),
                Direction::Backward,
                cancellation,
            )
        }
    }
}

struct Calendar {
    cron: Cron,
    zone: Tz,
}

impl Calendar {
    fn new(expression: &str, time_zone: &str) -> Result<Self, ClockError> {
        Ok(Self {
            cron: parse_cron(expression).map_err(invalid_schedule)?,
            zone: parse_time_zone(time_zone).map_err(invalid_schedule)?,
        })
    }

    fn search(
        &self,
        mut cursor: NaiveDateTime,
        boundary: DateTime<Utc>,
        lower_bound: Option<DateTime<Utc>>,
        direction: Direction,
        cancellation: &CancellationToken,
    ) -> Result<Option<UnixMillis>, ClockError> {
        let mut budget = CalculationBudget::new();
        let mut inclusive = true;
        loop {
            budget.admit(cancellation)?;
            check_cron_year(cursor)?;
            let found = match direction {
                Direction::Forward => self.cron.find_next_occurrence(&cursor, inclusive),
                Direction::Backward => self.cron.find_previous_occurrence(&cursor, inclusive),
            };
            budget.check(cancellation)?;
            let candidate = found
                .map_err(|_| ClockError::Blocked("calendar library search could not complete"))?;
            check_cron_year(candidate)?;
            if !direction.progressed(cursor, candidate, inclusive) {
                return Err(ClockError::Blocked("calendar search did not progress"));
            }
            let mapped = match self.zone.from_local_datetime(&candidate) {
                LocalResult::Single(instant) => instant,
                LocalResult::Ambiguous(first, second) => first.min(second),
                LocalResult::None => {
                    let gap = GapInfo::new(&candidate, &self.zone)
                        .ok_or(ClockError::Blocked("timezone gap information unavailable"))?;
                    match direction {
                        Direction::Forward => {
                            cursor = gap
                                .end
                                .ok_or(ClockError::Blocked("timezone gap has no upper boundary"))?
                                .naive_local();
                            if cursor <= candidate {
                                return Err(ClockError::Blocked("timezone gap did not progress"));
                            }
                            inclusive = true;
                        }
                        Direction::Backward => {
                            cursor = gap
                                .begin
                                .ok_or(ClockError::Blocked("timezone gap has no lower boundary"))?
                                .0;
                            if cursor > candidate {
                                return Err(ClockError::Blocked("timezone gap did not progress"));
                            }
                            inclusive = false;
                        }
                    }
                    continue;
                }
            };
            match direction {
                Direction::Forward if mapped > boundary => {
                    return Ok(Some(instant_millis(mapped)?));
                }
                Direction::Backward if mapped <= boundary => {
                    if lower_bound.is_some_and(|lower| mapped < lower) {
                        return Ok(None);
                    }
                    return Ok(Some(instant_millis(mapped)?));
                }
                _ => {}
            }
            cursor = candidate;
            inclusive = false;
        }
    }
}

#[derive(Clone, Copy)]
enum Direction {
    Forward,
    Backward,
}

impl Direction {
    fn progressed(self, cursor: NaiveDateTime, candidate: NaiveDateTime, inclusive: bool) -> bool {
        match self {
            Self::Forward => candidate > cursor || inclusive && candidate == cursor,
            Self::Backward => candidate < cursor || inclusive && candidate == cursor,
        }
    }
}

struct CalculationBudget {
    started: Instant,
    candidates: usize,
}

impl CalculationBudget {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            candidates: 0,
        }
    }

    fn admit(&mut self, cancellation: &CancellationToken) -> Result<(), ClockError> {
        self.check(cancellation)?;
        if self.candidates >= MAX_CANDIDATES {
            return Err(ClockError::Blocked("calendar candidate budget exceeded"));
        }
        self.candidates += 1;
        Ok(())
    }

    fn check(&self, cancellation: &CancellationToken) -> Result<(), ClockError> {
        check_cancelled(cancellation)?;
        if self.started.elapsed() >= MAX_CALCULATION_TIME {
            return Err(ClockError::Blocked("calendar elapsed-work budget exceeded"));
        }
        Ok(())
    }
}

fn validate(spec: &ScheduleSpec, boundary: UnixMillis) -> Result<(), ClockError> {
    timestamp(boundary).map_err(invalid_schedule)?;
    spec.validate().map_err(invalid_schedule)
}

fn invalid_schedule(error: ScheduleError) -> ClockError {
    match error {
        ScheduleError::Invalid(message) => ClockError::Invalid(message),
        _ => ClockError::Invalid("invalid schedule definition"),
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), ClockError> {
    if cancellation.is_cancelled() {
        return Err(ClockError::Cancelled);
    }
    Ok(())
}

fn check_cron_year(value: NaiveDateTime) -> Result<(), ClockError> {
    if !(croner::YEAR_LOWER_LIMIT..=croner::YEAR_UPPER_LIMIT).contains(&value.year()) {
        return Err(ClockError::Blocked("calendar exceeds library year range"));
    }
    Ok(())
}

fn integral_second(value: NaiveDateTime) -> Result<NaiveDateTime, ClockError> {
    value
        .with_nanosecond(0)
        .ok_or(ClockError::Invalid("invalid calendar second"))
}

fn utc(value: UnixMillis) -> Result<DateTime<Utc>, ClockError> {
    DateTime::from_timestamp_millis(timestamp(value).map_err(invalid_schedule)?)
        .ok_or(ClockError::Invalid("timestamp outside calendar range"))
}

fn checked_millis(value: u64) -> Result<UnixMillis, ClockError> {
    if value > MAX_TIMESTAMP {
        return Err(ClockError::Invalid("timestamp exceeds year 9999"));
    }
    UnixMillis::try_from(value).map_err(|_| ClockError::Invalid("timestamp overflow"))
}

fn instant_millis(value: DateTime<Tz>) -> Result<UnixMillis, ClockError> {
    checked_millis(
        u64::try_from(value.timestamp_millis())
            .map_err(|_| ClockError::Invalid("calendar instant precedes Unix epoch"))?,
    )
}
