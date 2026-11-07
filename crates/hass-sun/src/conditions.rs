//! Pure Home Assistant Sun condition calculations.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;

use crate::{Observer, SolarError, SolarEvent, event_on_date, next_event, position_at};

const HORIZON: f64 = -0.833;
const CIVIL: f64 = -6.0;
const NAUTICAL: f64 = -12.0;
const ASTRONOMICAL: f64 = -18.0;

/// Elevation band used by morning and evening twilight conditions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TwilightBand {
    /// Entire twilight span from astronomical dawn through the apparent horizon.
    Any,
    /// Civil twilight, from -6 degrees through the apparent horizon.
    Civil,
    /// Nautical twilight, from -12 through -6 degrees.
    Nautical,
    /// Astronomical twilight, from -18 through -12 degrees.
    Astronomical,
}

impl TwilightBand {
    fn bounds(self) -> (f64, f64) {
        match self {
            Self::Any => (ASTRONOMICAL, HORIZON),
            Self::Civil => (CIVIL, HORIZON),
            Self::Nautical => (NAUTICAL, CIVIL),
            Self::Astronomical => (ASTRONOMICAL, NAUTICAL),
        }
    }
}

/// Morning/evening selector for golden and blue hour.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SunPeriod {
    /// Either pass through the elevation band.
    Any,
    /// The rising pass.
    Morning,
    /// The setting pass.
    Evening,
}

/// Event names accepted by the legacy `sun` condition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacySunEvent {
    Sunrise,
    Sunset,
}

/// Inputs to the legacy before/after condition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegacySunCondition {
    pub before: Option<LegacySunEvent>,
    pub after: Option<LegacySunEvent>,
    pub before_offset: Duration,
    pub after_offset: Duration,
}

impl Default for LegacySunCondition {
    fn default() -> Self {
        Self {
            before: None,
            after: None,
            before_offset: Duration::zero(),
            after_offset: Duration::zero(),
        }
    }
}

/// Pure native condition variants supported by the Sun integration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PureSunCondition {
    IsUp,
    IsSet,
    IsAscending,
    IsDescending,
    IsNight,
    MorningTwilight(TwilightBand),
    EveningTwilight(TwilightBand),
    GoldenHour(SunPeriod),
    BlueHour(SunPeriod),
    IsMidnightSun,
    IsPolarNight,
    /// Strictly above the specified elevation, in degrees.
    ElevationAbove(f64),
    /// Strictly below the specified elevation, in degrees.
    ElevationBelow(f64),
    Legacy(LegacySunCondition),
}

/// Backward-compatible name for [`PureSunCondition`].
pub type SunCondition = PureSunCondition;

/// Evaluate a native Sun condition from the current observer and UTC instant.
pub fn evaluate_condition(
    observer: Observer,
    now: DateTime<Utc>,
    timezone: Tz,
    condition: PureSunCondition,
) -> Result<bool, SolarError> {
    match condition {
        PureSunCondition::IsUp => is_up(observer, now, timezone),
        PureSunCondition::IsSet => Ok(!is_up(observer, now, timezone)?),
        PureSunCondition::IsAscending => Ok(is_rising(observer, now)?),
        PureSunCondition::IsDescending => Ok(!is_rising(observer, now)?),
        PureSunCondition::IsNight => Ok(current_elevation(observer, now)? <= ASTRONOMICAL),
        PureSunCondition::MorningTwilight(band) => twilight_matches(observer, now, band, true),
        PureSunCondition::EveningTwilight(band) => twilight_matches(observer, now, band, false),
        PureSunCondition::GoldenHour(period) => hour_matches(observer, now, -4.0, 6.0, period),
        PureSunCondition::BlueHour(period) => hour_matches(observer, now, -6.0, -4.0, period),
        PureSunCondition::IsMidnightSun => {
            Ok(last_extreme_elevation(observer, now, timezone, SolarEvent::Midnight)? > HORIZON)
        }
        PureSunCondition::IsPolarNight => {
            Ok(last_extreme_elevation(observer, now, timezone, SolarEvent::Noon)? < HORIZON)
        }
        PureSunCondition::ElevationAbove(threshold) => {
            validate_threshold(threshold)?;
            Ok(current_elevation(observer, now)? > threshold)
        }
        PureSunCondition::ElevationBelow(threshold) => {
            validate_threshold(threshold)?;
            Ok(current_elevation(observer, now)? < threshold)
        }
        PureSunCondition::Legacy(condition) => {
            evaluate_legacy_sun_condition(observer, now, timezone, condition)
        }
    }
}

/// Evaluate the original public condition name.
pub fn evaluate_sun_condition(
    observer: Observer,
    now: DateTime<Utc>,
    timezone: Tz,
    condition: SunCondition,
) -> Result<bool, SolarError> {
    evaluate_condition(observer, now, timezone, condition)
}

/// Evaluate the legacy `sun` condition using local-date sunrise/sunset events.
///
/// A missing event on the selected local date makes a condition that refers to
/// that event false, as Home Assistant does in polar day/night. Before/after
/// comparisons are inclusive at the offset event instant. The special
/// `before sunrise` plus `after sunset` dark window uses OR comparisons with
/// strict inequalities.
pub fn evaluate_legacy_sun_condition(
    observer: Observer,
    now: DateTime<Utc>,
    timezone: Tz,
    condition: LegacySunCondition,
) -> Result<bool, SolarError> {
    let local_today = now.with_timezone(&timezone).date_naive();
    let has_sunrise = matches!(condition.before, Some(LegacySunEvent::Sunrise))
        || matches!(condition.after, Some(LegacySunEvent::Sunrise));
    let has_sunset = matches!(condition.before, Some(LegacySunEvent::Sunset))
        || matches!(condition.after, Some(LegacySunEvent::Sunset));

    let mut sunrise = event_on_date(observer, local_today, SolarEvent::Sunrise)?;
    let mut sunset = event_on_date(observer, local_today, SolarEvent::Sunset)?;

    // Astral returns UTC datetimes for a supplied calendar date. At extreme
    // longitude/time-zone combinations, an event may convert to another local
    // date; the legacy helper then evaluates tomorrow's event for that bound.
    if has_sunrise && event_is_before_local_date(sunrise, local_today, timezone) {
        sunrise = event_on_date(observer, next_date(local_today)?, SolarEvent::Sunrise)?;
    }
    if has_sunset && event_is_before_local_date(sunset, local_today, timezone) {
        sunset = event_on_date(observer, next_date(local_today)?, SolarEvent::Sunset)?;
    }

    if (has_sunrise && sunrise.is_none()) || (has_sunset && sunset.is_none()) {
        return Ok(false);
    }

    if condition.before == Some(LegacySunEvent::Sunrise)
        && condition.after == Some(LegacySunEvent::Sunset)
    {
        let before = offset_event(sunrise.expect("checked sunrise"), condition.before_offset)?;
        let after = offset_event(sunset.expect("checked sunset"), condition.after_offset)?;
        return Ok(now < before || now > after);
    }

    if let Some(event) = condition.before {
        let instant = event_for_condition(event, sunrise, sunset, local_today)?;
        if now > offset_event(instant, condition.before_offset)? {
            return Ok(false);
        }
    }
    if let Some(event) = condition.after {
        let instant = event_for_condition(event, sunrise, sunset, local_today)?;
        if now < offset_event(instant, condition.after_offset)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn event_is_before_local_date(
    event: Option<DateTime<Utc>>,
    local_today: NaiveDate,
    timezone: Tz,
) -> bool {
    event.is_some_and(|instant| instant.with_timezone(&timezone).date_naive() < local_today)
}

fn event_for_condition(
    event: LegacySunEvent,
    sunrise: Option<DateTime<Utc>>,
    sunset: Option<DateTime<Utc>>,
    date: NaiveDate,
) -> Result<DateTime<Utc>, SolarError> {
    match event {
        LegacySunEvent::Sunrise => sunrise,
        LegacySunEvent::Sunset => sunset,
    }
    .ok_or(SolarError::SearchExhausted {
        event: match event {
            LegacySunEvent::Sunrise => SolarEvent::Sunrise,
            LegacySunEvent::Sunset => SolarEvent::Sunset,
        },
        first_date: date,
        last_date: date,
    })
}

fn offset_event(event: DateTime<Utc>, offset: Duration) -> Result<DateTime<Utc>, SolarError> {
    event
        .checked_add_signed(offset)
        .ok_or(SolarError::DateOverflow)
}

fn next_date(date: NaiveDate) -> Result<NaiveDate, SolarError> {
    date.succ_opt().ok_or(SolarError::DateOverflow)
}

fn is_up(observer: Observer, now: DateTime<Utc>, timezone: Tz) -> Result<bool, SolarError> {
    let next_rising = next_event(
        observer,
        now,
        timezone,
        SolarEvent::Sunrise,
        Duration::zero(),
    )?;
    let next_setting = next_event(
        observer,
        now,
        timezone,
        SolarEvent::Sunset,
        Duration::zero(),
    )?;
    Ok(next_setting < next_rising)
}

fn is_rising(observer: Observer, now: DateTime<Utc>) -> Result<bool, SolarError> {
    let current = current_elevation(observer, now)?;
    let one_minute_later = now
        .checked_add_signed(Duration::minutes(1))
        .ok_or(SolarError::DateOverflow)?;
    Ok(current_elevation(observer, one_minute_later)? > current)
}

fn current_elevation(observer: Observer, now: DateTime<Utc>) -> Result<f64, SolarError> {
    Ok(position_at(observer, now)?.elevation)
}

fn twilight_matches(
    observer: Observer,
    now: DateTime<Utc>,
    band: TwilightBand,
    rising: bool,
) -> Result<bool, SolarError> {
    let elevation = current_elevation(observer, now)?;
    let (low, high) = band.bounds();
    Ok(is_rising(observer, now)? == rising && low <= elevation && elevation <= high)
}

fn hour_matches(
    observer: Observer,
    now: DateTime<Utc>,
    low: f64,
    high: f64,
    period: SunPeriod,
) -> Result<bool, SolarError> {
    let elevation = current_elevation(observer, now)?;
    if !(low <= elevation && elevation <= high) {
        return Ok(false);
    }
    match period {
        SunPeriod::Any => Ok(true),
        SunPeriod::Morning => is_rising(observer, now),
        SunPeriod::Evening => Ok(!is_rising(observer, now)?),
    }
}

fn last_extreme_elevation(
    observer: Observer,
    now: DateTime<Utc>,
    timezone: Tz,
    event: SolarEvent,
) -> Result<f64, SolarError> {
    let local_today = now.with_timezone(&timezone).date_naive();
    let first_date = local_today
        .checked_sub_signed(Duration::days(2))
        .ok_or(SolarError::DateOverflow)?;
    let last_date = local_today
        .checked_add_signed(Duration::days(2))
        .ok_or(SolarError::DateOverflow)?;
    let mut date = first_date;
    let mut latest = event_on_date(observer, date, event)?.ok_or(SolarError::SearchExhausted {
        event,
        first_date,
        last_date,
    })?;

    for _ in 0..4 {
        date = next_date(date)?;
        let candidate =
            event_on_date(observer, date, event)?.ok_or(SolarError::SearchExhausted {
                event,
                first_date,
                last_date,
            })?;
        if candidate <= now {
            latest = candidate;
        } else {
            break;
        }
    }
    Ok(position_at(observer, latest)?.elevation)
}

fn validate_threshold(threshold: f64) -> Result<(), SolarError> {
    if threshold.is_finite() {
        Ok(())
    } else {
        Err(SolarError::NonFiniteParameter("elevation threshold"))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, NaiveDate, TimeZone, Utc};
    use chrono_tz::{America::Anchorage, Europe::Oslo};

    use super::{
        LegacySunCondition, LegacySunEvent, PureSunCondition, SunCondition, SunPeriod,
        TwilightBand, evaluate_condition, evaluate_legacy_sun_condition, evaluate_sun_condition,
    };
    use crate::{Observer, SolarEvent};

    fn make_observer(latitude: f64, longitude: f64) -> Observer {
        Observer {
            latitude,
            longitude,
            elevation_m: 0.0,
        }
    }

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn up_set_and_direction_conditions_use_real_solar_position() {
        let observer = make_observer(32.87336, -117.22743);
        let dawn = utc(2015, 9, 15, 12, 0);
        let afternoon = utc(2015, 9, 15, 20, 0);
        assert!(!evaluate_sun_condition(observer, dawn, Anchorage, SunCondition::IsUp).unwrap());
        assert!(evaluate_sun_condition(observer, dawn, Anchorage, SunCondition::IsSet).unwrap());
        assert!(
            evaluate_sun_condition(observer, dawn, Anchorage, SunCondition::IsAscending).unwrap()
        );
        assert!(
            evaluate_sun_condition(observer, afternoon, Anchorage, SunCondition::IsDescending)
                .unwrap()
        );
    }

    #[test]
    fn legacy_condition_preserves_offsets_overnight_or_and_polar_false() {
        let observer = make_observer(32.87336, -117.22743);
        let condition = LegacySunCondition {
            before: Some(LegacySunEvent::Sunrise),
            after: Some(LegacySunEvent::Sunset),
            before_offset: chrono::Duration::zero(),
            after_offset: chrono::Duration::zero(),
        };
        assert!(
            evaluate_legacy_sun_condition(observer, utc(2015, 9, 15, 12, 0), Anchorage, condition,)
                .unwrap()
        );
        assert!(!evaluate_legacy_sun_condition(
            observer,
            utc(2015, 9, 15, 19, 0),
            Anchorage,
            condition,
        )
        .unwrap());
        assert!(
            evaluate_legacy_sun_condition(observer, utc(2015, 9, 15, 3, 0), Anchorage, condition,)
                .unwrap()
        );

        let polar = make_observer(69.6, 18.8);
        let no_sunrise = LegacySunCondition {
            before: Some(LegacySunEvent::Sunrise),
            ..LegacySunCondition::default()
        };
        assert!(
            !evaluate_legacy_sun_condition(polar, utc(2016, 6, 1, 12, 0), Oslo, no_sunrise,)
                .unwrap()
        );
    }

    #[test]
    fn twilight_hour_and_polar_conditions_use_inclusive_bands_and_extrema() {
        let observer = make_observer(32.87336, -117.22743);
        let morning_civil = utc(2015, 9, 15, 13, 15);
        assert!(
            evaluate_sun_condition(
                observer,
                morning_civil,
                Anchorage,
                SunCondition::MorningTwilight(TwilightBand::Civil),
            )
            .unwrap()
        );
        assert!(
            evaluate_sun_condition(
                observer,
                morning_civil,
                Anchorage,
                SunCondition::BlueHour(SunPeriod::Any),
            )
            .unwrap()
        );
        assert!(
            evaluate_sun_condition(
                observer,
                utc(2015, 9, 15, 13, 20),
                Anchorage,
                SunCondition::GoldenHour(SunPeriod::Morning),
            )
            .unwrap()
        );

        let svalbard = make_observer(78.22, 15.65);
        assert!(
            evaluate_sun_condition(
                svalbard,
                utc(2015, 6, 15, 12, 0),
                Oslo,
                SunCondition::IsMidnightSun,
            )
            .unwrap()
        );
        assert!(
            evaluate_sun_condition(
                svalbard,
                utc(2015, 12, 15, 12, 0),
                Oslo,
                SunCondition::IsPolarNight,
            )
            .unwrap()
        );
    }

    #[test]
    fn pure_condition_api_keeps_strict_elevation_thresholds() {
        let observer = make_observer(32.87336, -117.22743);
        let now = utc(2015, 9, 15, 19, 0);
        let elevation = crate::position_at(observer, now).unwrap().elevation;

        assert!(
            !evaluate_condition(
                observer,
                now,
                Anchorage,
                PureSunCondition::ElevationAbove(elevation),
            )
            .unwrap()
        );
        assert!(
            !evaluate_condition(
                observer,
                now,
                Anchorage,
                PureSunCondition::ElevationBelow(elevation),
            )
            .unwrap()
        );
        assert!(
            evaluate_condition(
                observer,
                now,
                Anchorage,
                PureSunCondition::Legacy(LegacySunCondition::default()),
            )
            .unwrap()
        );
    }

    #[test]
    fn legacy_boundaries_are_inclusive_after_offset_and_dst_keeps_utc_instants() {
        let observer = make_observer(32.87336, -117.22743);
        let local_date = NaiveDate::from_ymd_opt(2015, 9, 15).unwrap();
        let sunrise = crate::event_on_date(observer, local_date, SolarEvent::Sunrise)
            .unwrap()
            .unwrap();
        let condition = LegacySunCondition {
            before: Some(LegacySunEvent::Sunrise),
            before_offset: Duration::minutes(-15),
            ..LegacySunCondition::default()
        };
        let boundary = sunrise - Duration::minutes(15);
        assert!(evaluate_legacy_sun_condition(observer, boundary, Anchorage, condition).unwrap());
        assert!(
            !evaluate_legacy_sun_condition(
                observer,
                boundary + Duration::seconds(1),
                Anchorage,
                condition,
            )
            .unwrap()
        );

        let oslo = "Europe/Oslo".parse::<chrono_tz::Tz>().unwrap();
        for (date, expected_offset) in [
            (NaiveDate::from_ymd_opt(2024, 3, 31).unwrap(), "+02:00"),
            (NaiveDate::from_ymd_opt(2024, 10, 27).unwrap(), "+01:00"),
        ] {
            let event = crate::event_on_date(observer, date, SolarEvent::Sunrise)
                .unwrap()
                .unwrap();
            assert_eq!(
                event.with_timezone(&oslo).format("%:z").to_string(),
                expected_offset
            );
        }
    }
}
