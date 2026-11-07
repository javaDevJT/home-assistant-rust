//! Pure Rust solar calculations compatible with Home Assistant's pinned
//! Astral 2.2 behavior.
//!
//! The implementation follows Astral 2.2's NOAA-based equations, transit
//! iteration, horizon/refraction treatment, and microsecond conversion. It
//! intentionally does not use an external astronomy library or Python runtime.
//! See [`NOTICE`](../NOTICE) for source and license attribution.

#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

mod conditions;
mod integration;

pub use conditions::{
    LegacySunCondition, LegacySunEvent, PureSunCondition, SunCondition, SunPeriod, TwilightBand,
    evaluate_condition, evaluate_legacy_sun_condition, evaluate_sun_condition,
};
pub use integration::{SunIntegration, SunManager, SunPhase, SunSnapshot};

const SUN_APPARENT_RADIUS: f64 = 32.0 / (60.0 * 2.0);
const EARTH_RADIUS_METERS: f64 = 6_356_900.0;
const MINUTES_PER_DAY: f64 = 1_440.0;
const MICROS_PER_SECOND: f64 = 1_000_000.0;
const MICROS_PER_DAY: f64 = 86_400_000_000.0;

/// Geographic observer used by the Home Assistant Sun integration.
///
/// `elevation_m` is passed directly from Home Assistant's configured elevation
/// to Astral's numeric observer elevation. Astral uses positive values to
/// extend the visible horizon for event times; it does not apply that value to
/// [`position_at`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observer {
    /// North is positive, in degrees.
    pub latitude: f64,
    /// East is positive, in degrees.
    pub longitude: f64,
    /// Observer elevation in meters, passed through as Astral 2.2 does.
    pub elevation_m: f64,
}

/// Solar position in degrees. Values are raw and unrounded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    /// Degrees above the horizon, including Astral's refraction correction.
    pub elevation: f64,
    /// Degrees clockwise from north, in the range `[0, 360)`.
    pub azimuth: f64,
}

/// A date-scoped solar event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SolarEvent {
    /// Sun center crosses the apparent horizon while rising.
    Sunrise,
    /// Sun center crosses the apparent horizon while setting.
    Sunset,
    /// Rising crossing at the specified positive-below-horizon depression.
    /// Negative values request a crossing above the horizon.
    Dawn { depression: f64 },
    /// Setting crossing at the specified positive-below-horizon depression.
    /// Negative values request a crossing above the horizon.
    Dusk { depression: f64 },
    /// Solar transit at the daily maximum elevation.
    Noon,
    /// Solar transit at the daily minimum elevation.
    Midnight,
    /// Crossing of a specified solar elevation. `rising` selects the branch.
    Elevation { elevation: f64, rising: bool },
}

impl SolarEvent {
    fn label(self) -> &'static str {
        match self {
            Self::Sunrise => "sunrise",
            Self::Sunset => "sunset",
            Self::Dawn { .. } => "dawn",
            Self::Dusk { .. } => "dusk",
            Self::Noon => "noon",
            Self::Midnight => "midnight",
            Self::Elevation { .. } => "elevation crossing",
        }
    }

    fn validate(self) -> Result<(), SolarError> {
        match self {
            Self::Dawn { depression } | Self::Dusk { depression } if !depression.is_finite() => {
                Err(SolarError::NonFiniteParameter("depression"))
            }
            Self::Elevation { elevation, .. } if !elevation.is_finite() => {
                Err(SolarError::NonFiniteParameter("solar elevation"))
            }
            _ => Ok(()),
        }
    }
}

/// Validation, date-range, or bounded-search error.
#[derive(Debug, Clone, PartialEq)]
pub enum SolarError {
    /// Observer latitude must be finite and in `[-90, 90]` degrees.
    InvalidLatitude(f64),
    /// Observer longitude must be finite and in `[-180, 180]` degrees.
    InvalidLongitude(f64),
    /// Observer elevation must be finite.
    NonFiniteObserverElevation(f64),
    /// A solar event angle must be finite.
    NonFiniteParameter(&'static str),
    /// Astral's source uses Python datetimes, whose supported years are 1–9999.
    DateOutsideAstralRange(i32),
    /// Date arithmetic exceeded the supported range.
    DateOverflow,
    /// No requested event was found in the Home Assistant one-year scan.
    SearchExhausted {
        event: SolarEvent,
        first_date: NaiveDate,
        last_date: NaiveDate,
    },
    /// A position calculation produced a non-finite result.
    NonFiniteResult,
}

impl fmt::Display for SolarError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLatitude(value) => {
                write!(formatter, "latitude {value} is outside [-90, 90] degrees")
            }
            Self::InvalidLongitude(value) => {
                write!(
                    formatter,
                    "longitude {value} is outside [-180, 180] degrees"
                )
            }
            Self::NonFiniteObserverElevation(value) => {
                write!(formatter, "observer elevation {value} is not finite")
            }
            Self::NonFiniteParameter(parameter) => {
                write!(formatter, "{parameter} is not finite")
            }
            Self::DateOutsideAstralRange(year) => {
                write!(
                    formatter,
                    "year {year} is outside Astral's supported range 1..=9999"
                )
            }
            Self::DateOverflow => formatter.write_str("solar date arithmetic overflowed"),
            Self::SearchExhausted {
                event,
                first_date,
                last_date,
            } => write!(
                formatter,
                "unable to find {} after one year (scanned {first_date} through {last_date})",
                event.label()
            ),
            Self::NonFiniteResult => formatter.write_str("solar position was not finite"),
        }
    }
}

impl Error for SolarError {}

/// Calculate the raw solar elevation and azimuth at a UTC instant.
///
/// As in Astral 2.2, the observer's elevation is not used to adjust position,
/// azimuth is calculated without refraction, and elevation includes the
/// piecewise atmospheric refraction correction. Fractional seconds are ignored
/// by the pinned algorithm.
pub fn position_at(observer: Observer, at: DateTime<Utc>) -> Result<Position, SolarError> {
    validate_observer(observer)?;
    validate_date(at.date_naive())?;

    let (zenith, azimuth) = zenith_and_azimuth(observer, at)?;
    let elevation = 90.0 - zenith;
    if elevation.is_finite() && azimuth.is_finite() {
        Ok(Position { elevation, azimuth })
    } else {
        Err(SolarError::NonFiniteResult)
    }
}

/// Calculate an event for one calendar date, returning `None` when Astral's
/// equations say the event does not occur on that date.
pub fn event_on_date(
    observer: Observer,
    date: NaiveDate,
    event: SolarEvent,
) -> Result<Option<DateTime<Utc>>, SolarError> {
    validate_observer(observer)?;
    validate_date(date)?;
    event.validate()?;

    let result = match event {
        SolarEvent::Sunrise => time_of_transit(
            observer,
            date,
            90.0 + SUN_APPARENT_RADIUS,
            SunDirection::Rising,
        ),
        SolarEvent::Sunset => time_of_transit(
            observer,
            date,
            90.0 + SUN_APPARENT_RADIUS,
            SunDirection::Setting,
        ),
        SolarEvent::Dawn { depression } => {
            time_of_transit(observer, date, 90.0 + depression, SunDirection::Rising)
        }
        SolarEvent::Dusk { depression } => {
            time_of_transit(observer, date, 90.0 + depression, SunDirection::Setting)
        }
        SolarEvent::Noon => Some(solar_noon(observer, date)?),
        SolarEvent::Midnight => Some(solar_midnight(observer, date)?),
        SolarEvent::Elevation { elevation, rising } => {
            let (elevation, direction) = if elevation > 90.0 {
                (180.0 - elevation, SunDirection::Setting)
            } else if rising {
                (elevation, SunDirection::Rising)
            } else {
                (elevation, SunDirection::Setting)
            };
            time_of_transit(observer, date, 90.0 - elevation, direction)
        }
    };

    Ok(result)
}

/// Find the next event using Home Assistant's local-date scan and strict
/// `event + offset > now` comparison.
///
/// The scan examines the previous local calendar date through local date +366
/// (368 dates, matching HA's `mod = -1; while mod < 367` loop). Event results
/// remain UTC instants; timezone is used only to select the date sequence.
pub fn next_event(
    observer: Observer,
    now: DateTime<Utc>,
    timezone: Tz,
    event: SolarEvent,
    offset: Duration,
) -> Result<DateTime<Utc>, SolarError> {
    validate_observer(observer)?;
    event.validate()?;
    validate_date(now.date_naive())?;

    let local_today = now.with_timezone(&timezone).date_naive();
    let first_date = local_today
        .checked_sub_signed(Duration::days(1))
        .ok_or(SolarError::DateOverflow)?;
    let last_date = local_today
        .checked_add_signed(Duration::days(366))
        .ok_or(SolarError::DateOverflow)?;

    for days in -1_i64..=366_i64 {
        let date = local_today
            .checked_add_signed(Duration::days(days))
            .ok_or(SolarError::DateOverflow)?;
        if let Some(event_at) = event_on_date(observer, date, event)? {
            let fire_at = event_at
                .checked_add_signed(offset)
                .ok_or(SolarError::DateOverflow)?;
            if fire_at > now {
                return Ok(fire_at);
            }
        }
    }

    Err(SolarError::SearchExhausted {
        event,
        first_date,
        last_date,
    })
}

#[derive(Clone, Copy)]
enum SunDirection {
    Rising,
    Setting,
}

fn validate_observer(observer: Observer) -> Result<(), SolarError> {
    if !observer.latitude.is_finite() || !(-90.0..=90.0).contains(&observer.latitude) {
        return Err(SolarError::InvalidLatitude(observer.latitude));
    }
    if !observer.longitude.is_finite() || !(-180.0..=180.0).contains(&observer.longitude) {
        return Err(SolarError::InvalidLongitude(observer.longitude));
    }
    if !observer.elevation_m.is_finite() {
        return Err(SolarError::NonFiniteObserverElevation(observer.elevation_m));
    }
    Ok(())
}

fn validate_date(date: NaiveDate) -> Result<(), SolarError> {
    if !(1..=9999).contains(&date.year()) {
        return Err(SolarError::DateOutsideAstralRange(date.year()));
    }
    Ok(())
}

fn julianday(date: NaiveDate) -> f64 {
    let mut year = f64::from(date.year());
    let mut month = i32::try_from(date.month()).expect("month is in 1..=12");
    let day = f64::from(date.day());
    if month <= 2 {
        year -= 1.0;
        month += 12;
    }

    let a = (year / 100.0).floor();
    let b = 2.0 - a + (a / 4.0).floor();
    (365.25 * (year + 4716.0)).floor() + (30.6001 * f64::from(month + 1)).floor() + day + b - 1524.5
}

fn jday_to_jcentury(julianday: f64) -> f64 {
    (julianday - 2_451_545.0) / 36_525.0
}

fn jcentury_to_jday(juliancentury: f64) -> f64 {
    (juliancentury * 36_525.0) + 2_451_545.0
}

fn geom_mean_long_sun(juliancentury: f64) -> f64 {
    let longitude = 280.46646 + juliancentury * (36_000.769_83 + 0.0003032 * juliancentury);
    longitude.rem_euclid(360.0)
}

fn geom_mean_anomaly_sun(juliancentury: f64) -> f64 {
    357.52911 + juliancentury * (35_999.050_29 - 0.0001537 * juliancentury)
}

fn eccentric_location_earth_orbit(juliancentury: f64) -> f64 {
    0.016708634 - juliancentury * (0.000042037 + 0.0000001267 * juliancentury)
}

fn sun_eq_of_center(juliancentury: f64) -> f64 {
    let anomaly = geom_mean_anomaly_sun(juliancentury);
    let anomaly_rad = radians(anomaly);
    let sin_m = anomaly_rad.sin();
    let sin_2m = (anomaly_rad + anomaly_rad).sin();
    let sin_3m = (anomaly_rad + anomaly_rad + anomaly_rad).sin();
    sin_m * (1.914602 - juliancentury * (0.004817 + 0.000014 * juliancentury))
        + sin_2m * (0.019993 - 0.000101 * juliancentury)
        + sin_3m * 0.000289
}

fn sun_true_long(juliancentury: f64) -> f64 {
    geom_mean_long_sun(juliancentury) + sun_eq_of_center(juliancentury)
}

fn sun_apparent_long(juliancentury: f64) -> f64 {
    let true_longitude = sun_true_long(juliancentury);
    let omega = 125.04 - 1_934.136 * juliancentury;
    true_longitude - 0.00569 - 0.00478 * radians(omega).sin()
}

fn mean_obliquity_of_ecliptic(juliancentury: f64) -> f64 {
    let seconds =
        21.448 - juliancentury * (46.815 + juliancentury * (0.00059 - juliancentury * 0.001813));
    23.0 + (26.0 + seconds / 60.0) / 60.0
}

fn obliquity_correction(juliancentury: f64) -> f64 {
    let mean = mean_obliquity_of_ecliptic(juliancentury);
    let omega = 125.04 - 1_934.136 * juliancentury;
    mean + 0.00256 * radians(omega).cos()
}

fn sun_declination(juliancentury: f64) -> f64 {
    let obliquity = obliquity_correction(juliancentury);
    let apparent_longitude = sun_apparent_long(juliancentury);
    let sine = radians(obliquity).sin() * radians(apparent_longitude).sin();
    sine.asin().to_degrees()
}

fn var_y(juliancentury: f64) -> f64 {
    let epsilon = obliquity_correction(juliancentury);
    let tangent = (radians(epsilon) / 2.0).tan();
    tangent * tangent
}

fn eq_of_time(juliancentury: f64) -> f64 {
    let longitude = geom_mean_long_sun(juliancentury);
    let eccentricity = eccentric_location_earth_orbit(juliancentury);
    let anomaly = geom_mean_anomaly_sun(juliancentury);
    let y = var_y(juliancentury);
    let sin_2l0 = (2.0 * radians(longitude)).sin();
    let sin_m = radians(anomaly).sin();
    let cos_2l0 = (2.0 * radians(longitude)).cos();
    let sin_4l0 = (4.0 * radians(longitude)).sin();
    let sin_2m = (2.0 * radians(anomaly)).sin();
    let equation = y * sin_2l0 - 2.0 * eccentricity * sin_m
        + 4.0 * eccentricity * y * sin_m * cos_2l0
        - 0.5 * y * y * sin_4l0
        - 1.25 * eccentricity * eccentricity * sin_2m;
    equation.to_degrees() * 4.0
}

fn hour_angle(
    latitude: f64,
    declination: f64,
    zenith: f64,
    direction: SunDirection,
) -> Option<f64> {
    let latitude_rad = radians(latitude);
    let declination_rad = radians(declination);
    let zenith_rad = radians(zenith);
    let numerator = zenith_rad.cos() - latitude_rad.sin() * declination_rad.sin();
    let denominator = latitude_rad.cos() * declination_rad.cos();
    let mut hour_angle = safe_acos(numerator / denominator)?;
    if matches!(direction, SunDirection::Setting) {
        hour_angle = -hour_angle;
    }
    Some(hour_angle)
}

fn adjust_to_horizon(elevation_m: f64) -> f64 {
    if elevation_m <= 0.0 {
        return 0.0;
    }
    let radius = EARTH_RADIUS_METERS;
    let angle = safe_acos(radius / (radius + elevation_m)).unwrap_or(f64::NAN);
    angle.to_degrees()
}

fn refraction_at_zenith(zenith: f64) -> f64 {
    let elevation = 90.0 - zenith;
    if elevation >= 85.0 {
        return 0.0;
    }

    let correction = if elevation > 5.0 {
        let tangent = radians(elevation).tan();
        58.1 / tangent - 0.07 / tangent.powi(3) + 0.000086 / tangent.powi(5)
    } else if elevation > -0.575 {
        let step1 = -12.79 + elevation * 0.711;
        let step2 = 103.4 + elevation * step1;
        let step3 = -518.2 + elevation * step2;
        1_735.0 + elevation * step3
    } else {
        -20.774 / radians(elevation).tan()
    };
    correction / 3_600.0
}

fn time_of_transit(
    observer: Observer,
    date: NaiveDate,
    zenith: f64,
    direction: SunDirection,
) -> Option<DateTime<Utc>> {
    let latitude = observer.latitude.clamp(-89.8, 89.8);
    let elevation_adjustment = adjust_to_horizon(observer.elevation_m);
    if !elevation_adjustment.is_finite() {
        return None;
    }
    let refraction = refraction_at_zenith(zenith + elevation_adjustment);

    let mut juliancentury = jday_to_jcentury(julianday(date));
    let mut solar_declination = sun_declination(juliancentury);
    let hourangle = hour_angle(
        latitude,
        solar_declination,
        zenith + elevation_adjustment - refraction,
        direction,
    )?;
    let delta = -observer.longitude - hourangle.to_degrees();
    let time_diff = 4.0 * delta;
    let mut time_utc = 720.0 + time_diff - eq_of_time(juliancentury);

    juliancentury = jday_to_jcentury(jcentury_to_jday(juliancentury) + time_utc / MINUTES_PER_DAY);
    solar_declination = sun_declination(juliancentury);
    let hourangle = hour_angle(
        latitude,
        solar_declination,
        zenith + elevation_adjustment + refraction,
        direction,
    )?;
    let delta = -observer.longitude - hourangle.to_degrees();
    let time_diff = 4.0 * delta;
    time_utc = 720.0 + time_diff - eq_of_time(juliancentury);

    let duration = minutes_to_duration(time_utc)?;
    let midnight = date.and_hms_opt(0, 0, 0)?;
    let event = midnight.checked_add_signed(duration)?;
    Some(Utc.from_utc_datetime(&event))
}

fn minutes_to_duration(minutes: f64) -> Option<Duration> {
    if !minutes.is_finite() {
        return None;
    }
    // Keep Astral 2.2's int/truncation sequence; converting the whole value at
    // once can shift a microsecond fixture by one due to floating-point order.
    let days = (minutes / MINUTES_PER_DAY).trunc();
    let mut remainder_minutes = minutes - days * MINUTES_PER_DAY;
    remainder_minutes *= 60.0;
    let seconds = remainder_minutes.trunc();
    let fractional_seconds = remainder_minutes - seconds;
    let microseconds = (fractional_seconds * MICROS_PER_SECOND).trunc();
    let total_microseconds = days * MICROS_PER_DAY + seconds * MICROS_PER_SECOND + microseconds;
    if !total_microseconds.is_finite()
        || total_microseconds < i64::MIN as f64
        || total_microseconds > i64::MAX as f64
    {
        return None;
    }
    Some(Duration::microseconds(total_microseconds as i64))
}

fn solar_noon(observer: Observer, date: NaiveDate) -> Result<DateTime<Utc>, SolarError> {
    let century = jday_to_jcentury(julianday(date));
    let equation_of_time = eq_of_time(century);
    let time_utc = (720.0 - 4.0 * observer.longitude - equation_of_time) / 60.0;
    date_time_from_decimal_hours(date, time_utc, true).ok_or(SolarError::DateOverflow)
}

fn solar_midnight(observer: Observer, date: NaiveDate) -> Result<DateTime<Utc>, SolarError> {
    let julian_day = julianday(date);
    let new_century = jday_to_jcentury(julian_day + 0.5 - observer.longitude / 360.0);
    let equation_of_time = eq_of_time(new_century);
    let time_utc = (-observer.longitude * 4.0 - equation_of_time) / 60.0;
    date_time_from_decimal_hours(date, time_utc, false).ok_or(SolarError::DateOverflow)
}

fn date_time_from_decimal_hours(
    mut date: NaiveDate,
    time_hours: f64,
    normalize_after_midnight: bool,
) -> Option<DateTime<Utc>> {
    if !time_hours.is_finite() {
        return None;
    }
    let mut hour = time_hours.trunc() as i32;
    let mut minute = ((time_hours - f64::from(hour)) * 60.0).trunc() as i32;
    let mut second =
        ((((time_hours - f64::from(hour)) * 60.0) - f64::from(minute)) * 60.0).trunc() as i32;

    if second > 59 {
        second -= 60;
        minute += 1;
    } else if second < 0 {
        second += 60;
        minute -= 1;
    }
    if minute > 59 {
        minute -= 60;
        hour += 1;
    } else if minute < 0 {
        minute += 60;
        hour -= 1;
    }
    if normalize_after_midnight && hour > 23 {
        hour -= 24;
        date = date.succ_opt()?;
    } else if hour < 0 {
        hour += 24;
        date = date.pred_opt()?;
    }

    let result = date.and_hms_opt(hour as u32, minute as u32, second as u32)?;
    Some(Utc.from_utc_datetime(&result))
}

fn zenith_and_azimuth(observer: Observer, at: DateTime<Utc>) -> Result<(f64, f64), SolarError> {
    let latitude = observer.latitude.clamp(-89.8, 89.8);
    let longitude = observer.longitude;
    let utc = at.naive_utc();
    let time_now =
        f64::from(utc.hour()) + f64::from(utc.minute()) / 60.0 + f64::from(utc.second()) / 3_600.0;

    let century = jday_to_jcentury(julianday(utc.date()) + time_now / 24.0);
    let solar_declination = sun_declination(century);
    let equation_of_time = eq_of_time(century);
    let solar_time_fix = equation_of_time - (4.0 * -longitude);
    let mut true_solar_time = f64::from(utc.hour()) * 60.0
        + f64::from(utc.minute())
        + f64::from(utc.second()) / 60.0
        + solar_time_fix;
    while true_solar_time > MINUTES_PER_DAY {
        true_solar_time -= MINUTES_PER_DAY;
    }

    let mut hourangle = true_solar_time / 4.0 - 180.0;
    if hourangle < -180.0 {
        hourangle += 360.0;
    }
    let hourangle_rad = radians(hourangle);
    let csz = (radians(latitude).sin() * radians(solar_declination).sin()
        + radians(latitude).cos() * radians(solar_declination).cos() * hourangle_rad.cos())
    .clamp(-1.0, 1.0);
    let mut zenith = csz.acos().to_degrees();
    let azimuth_denominator = radians(latitude).cos() * radians(zenith).sin();

    let azimuth = if azimuth_denominator.abs() > 0.001 {
        let mut azimuth_radians = (radians(latitude).sin() * radians(zenith).cos()
            - radians(solar_declination).sin())
            / azimuth_denominator;
        azimuth_radians = azimuth_radians.clamp(-1.0, 1.0);
        let mut azimuth = 180.0 - azimuth_radians.acos().to_degrees();
        if hourangle > 0.0 {
            azimuth = -azimuth;
        }
        if azimuth < 0.0 {
            azimuth += 360.0;
        }
        azimuth
    } else if latitude > 0.0 {
        180.0
    } else {
        0.0
    };

    zenith -= refraction_at_zenith(zenith);
    Ok((zenith, azimuth))
}

fn safe_acos(value: f64) -> Option<f64> {
    if !value.is_finite() || !(-1.0..=1.0).contains(&value) {
        None
    } else {
        Some(value.acos())
    }
}

fn radians(degrees: f64) -> f64 {
    degrees.to_radians()
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, TimeZone, Utc};
    use chrono_tz::{America::Anchorage, Europe::Oslo};

    use super::{Observer, SolarError, SolarEvent, event_on_date, next_event, position_at};

    fn observer(latitude: f64, longitude: f64, elevation_m: f64) -> Observer {
        Observer {
            latitude,
            longitude,
            elevation_m,
        }
    }

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn utc(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
    ) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, second)
            .single()
            .unwrap()
    }

    #[test]
    fn astral_22_san_diego_dawn_dusk_reference_microseconds() {
        let observer = observer(32.87336, -117.22743, 0.0);
        let dawn_date = date(2015, 9, 15);
        let dawn_cases = [
            (
                SolarEvent::Dawn { depression: 6.0 },
                utc(2015, 9, 15, 13, 7, 26) + chrono::Duration::microseconds(84_892),
            ),
            (
                SolarEvent::Dawn { depression: 12.0 },
                utc(2015, 9, 15, 12, 38, 34) + chrono::Duration::microseconds(55_317),
            ),
            (
                SolarEvent::Dawn { depression: 18.0 },
                utc(2015, 9, 15, 12, 9, 9) + chrono::Duration::microseconds(742_065),
            ),
        ];
        for (event, expected) in dawn_cases {
            assert_eq!(
                event_on_date(observer, dawn_date, event).unwrap(),
                Some(expected)
            );
        }

        // Home Assistant selects the configured local calendar date. On
        // September 14 in San Diego, dusk occurs on September 15 in UTC.
        let dusk_date = date(2015, 9, 14);
        let dusk_cases = [
            (
                SolarEvent::Dusk { depression: 6.0 },
                utc(2015, 9, 15, 2, 21, 39) + chrono::Duration::microseconds(56_429),
            ),
            (
                SolarEvent::Dusk { depression: 12.0 },
                utc(2015, 9, 15, 2, 50, 29) + chrono::Duration::microseconds(596_023),
            ),
            (
                SolarEvent::Dusk { depression: 18.0 },
                utc(2015, 9, 15, 3, 19, 52) + chrono::Duration::microseconds(663_966),
            ),
        ];
        for (event, expected) in dusk_cases {
            assert_eq!(
                event_on_date(observer, dusk_date, event).unwrap(),
                Some(expected)
            );
        }
    }

    #[test]
    fn position_matches_astral_22_reference() {
        let observer = observer(32.87336, -117.22743, 0.0);
        let position = position_at(observer, utc(2015, 9, 15, 19, 0, 0)).unwrap();
        assert!((position.elevation - 58.328_698_962_961_944).abs() < 1.0e-11);
        assert!((position.azimuth - 158.651_543_458_069_06).abs() < 1.0e-11);
    }

    #[test]
    fn noon_and_midnight_match_astral_22_utc_truncation() {
        let observer = observer(32.87336, -117.22743, 0.0);
        let date = date(2015, 9, 15);
        assert_eq!(
            event_on_date(observer, date, SolarEvent::Noon).unwrap(),
            Some(utc(2015, 9, 15, 19, 44, 25))
        );
        assert_eq!(
            event_on_date(observer, date, SolarEvent::Midnight).unwrap(),
            Some(utc(2015, 9, 15, 7, 44, 8))
        );
    }

    #[test]
    fn explicit_elevation_crossings_match_dawn_dusk_and_wrap_over_90() {
        let observer = observer(32.87336, -117.22743, 0.0);
        let rising = event_on_date(
            observer,
            date(2015, 9, 15),
            SolarEvent::Elevation {
                elevation: -6.0,
                rising: true,
            },
        )
        .unwrap();
        let dawn = event_on_date(
            observer,
            date(2015, 9, 15),
            SolarEvent::Dawn { depression: 6.0 },
        )
        .unwrap();
        assert_eq!(rising, dawn);

        let setting = event_on_date(
            observer,
            date(2015, 9, 14),
            SolarEvent::Elevation {
                elevation: -6.0,
                rising: false,
            },
        )
        .unwrap();
        let dusk = event_on_date(
            observer,
            date(2015, 9, 14),
            SolarEvent::Dusk { depression: 6.0 },
        )
        .unwrap();
        assert_eq!(setting, dusk);

        let over_90 = event_on_date(
            observer,
            date(2015, 9, 15),
            SolarEvent::Elevation {
                elevation: 95.0,
                rising: true,
            },
        )
        .unwrap();
        let equivalent_setting = event_on_date(
            observer,
            date(2015, 9, 15),
            SolarEvent::Elevation {
                elevation: 85.0,
                rising: false,
            },
        )
        .unwrap();
        assert_eq!(over_90, equivalent_setting);
    }

    #[test]
    fn norway_polar_summer_returns_absent_date_and_next_events() {
        let observer = observer(69.6, 18.8, 0.0);
        let now = utc(2016, 6, 1, 0, 0, 0);
        assert_eq!(
            event_on_date(observer, date(2016, 6, 1), SolarEvent::Sunrise).unwrap(),
            None
        );
        assert_eq!(
            event_on_date(observer, date(2016, 6, 1), SolarEvent::Sunset).unwrap(),
            None
        );
        assert_eq!(
            next_event(
                observer,
                now,
                Oslo,
                SolarEvent::Sunrise,
                chrono::Duration::zero()
            )
            .unwrap(),
            utc(2016, 7, 24, 22, 59, 45) + chrono::Duration::microseconds(689_645)
        );
        assert_eq!(
            next_event(
                observer,
                now,
                Oslo,
                SolarEvent::Sunset,
                chrono::Duration::zero()
            )
            .unwrap(),
            utc(2016, 7, 25, 22, 17, 13) + chrono::Duration::microseconds(503_932)
        );
    }

    #[test]
    fn repeated_local_day_events_are_not_skipped() {
        let kotzebue = observer(66.8983, -162.5966, 0.0);
        let now = utc(2015, 8, 7, 7, 0, 0);
        let first = next_event(
            kotzebue,
            now,
            Anchorage,
            SolarEvent::Sunset,
            chrono::Duration::zero(),
        )
        .unwrap();
        let second = next_event(
            kotzebue,
            first,
            Anchorage,
            SolarEvent::Sunset,
            chrono::Duration::zero(),
        )
        .unwrap();
        assert_eq!(first.timestamp(), utc(2015, 8, 7, 8, 3, 42).timestamp());
        assert_eq!(second.timestamp(), utc(2015, 8, 8, 7, 59, 25).timestamp());
        assert_eq!(
            first.with_timezone(&Anchorage).date_naive(),
            second.with_timezone(&Anchorage).date_naive()
        );

        let synthetic = observer(66.5, -32.5, 0.0);
        let now = utc(2015, 3, 7, 9, 0, 0);
        let first = next_event(
            synthetic,
            now,
            Anchorage,
            SolarEvent::Sunrise,
            chrono::Duration::zero(),
        )
        .unwrap();
        let second = next_event(
            synthetic,
            first,
            Anchorage,
            SolarEvent::Sunrise,
            chrono::Duration::zero(),
        )
        .unwrap();
        assert_eq!(first.timestamp(), utc(2015, 3, 7, 9, 2, 36).timestamp());
        assert_eq!(second.timestamp(), utc(2015, 3, 8, 8, 58, 43).timestamp());
        assert_eq!(
            first.with_timezone(&Anchorage).date_naive(),
            second.with_timezone(&Anchorage).date_naive()
        );
    }

    #[test]
    fn next_event_keeps_utc_instants_across_oslo_dst_and_local_date_rolls() {
        let oslo_observer = observer(59.9139, 10.7522, 0.0);
        let spring_sunrise = utc(2024, 3, 31, 4, 44, 33) + chrono::Duration::microseconds(955_968);
        let fall_sunrise = utc(2024, 10, 27, 6, 27, 27) + chrono::Duration::microseconds(823_609);

        assert_eq!(
            utc(2024, 3, 31, 0, 59, 59)
                .with_timezone(&Oslo)
                .format("%F %T %:z")
                .to_string(),
            "2024-03-31 01:59:59 +01:00"
        );
        assert_eq!(
            utc(2024, 3, 31, 1, 0, 0)
                .with_timezone(&Oslo)
                .format("%F %T %:z")
                .to_string(),
            "2024-03-31 03:00:00 +02:00"
        );
        assert_eq!(
            utc(2024, 10, 27, 0, 59, 59)
                .with_timezone(&Oslo)
                .format("%F %T %:z")
                .to_string(),
            "2024-10-27 02:59:59 +02:00"
        );
        assert_eq!(
            utc(2024, 10, 27, 1, 0, 0)
                .with_timezone(&Oslo)
                .format("%F %T %:z")
                .to_string(),
            "2024-10-27 02:00:00 +01:00"
        );

        let spring_cases = [
            (
                utc(2024, 3, 30, 22, 59, 59),
                utc(2024, 3, 30, 23, 0, 0),
                spring_sunrise,
            ),
            (
                utc(2024, 3, 31, 0, 59, 59),
                utc(2024, 3, 31, 1, 0, 0),
                spring_sunrise,
            ),
        ];
        for (before, after, expected) in spring_cases {
            assert_eq!(
                next_event(
                    oslo_observer,
                    before,
                    Oslo,
                    SolarEvent::Sunrise,
                    chrono::Duration::zero(),
                )
                .unwrap(),
                expected
            );
            assert_eq!(
                next_event(
                    oslo_observer,
                    after,
                    Oslo,
                    SolarEvent::Sunrise,
                    chrono::Duration::zero(),
                )
                .unwrap(),
                expected
            );
        }

        let fall_cases = [
            (
                utc(2024, 10, 26, 21, 59, 59),
                utc(2024, 10, 26, 22, 0, 0),
                fall_sunrise,
            ),
            (
                utc(2024, 10, 27, 0, 59, 59),
                utc(2024, 10, 27, 1, 0, 0),
                fall_sunrise,
            ),
        ];
        for (before, after, expected) in fall_cases {
            assert_eq!(
                next_event(
                    oslo_observer,
                    before,
                    Oslo,
                    SolarEvent::Sunrise,
                    chrono::Duration::zero(),
                )
                .unwrap(),
                expected
            );
            assert_eq!(
                next_event(
                    oslo_observer,
                    after,
                    Oslo,
                    SolarEvent::Sunrise,
                    chrono::Duration::zero(),
                )
                .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn elevation_changes_event_horizon_but_not_position() {
        let ground = observer(32.87336, -117.22743, 0.0);
        let elevated = observer(32.87336, -117.22743, 1_000.0);
        let date = date(2015, 9, 15);
        let ground_sunrise = event_on_date(ground, date, SolarEvent::Sunrise)
            .unwrap()
            .unwrap();
        let elevated_sunrise = event_on_date(elevated, date, SolarEvent::Sunrise)
            .unwrap()
            .unwrap();
        assert!(elevated_sunrise < ground_sunrise);
        let at = utc(2015, 9, 15, 12, 0, 0);
        assert_eq!(
            position_at(ground, at).unwrap(),
            position_at(elevated, at).unwrap()
        );
    }

    #[test]
    fn polar_dates_keep_solar_noon_and_midnight_available() {
        let svalbard = observer(78.22, 15.65, 0.0);
        assert_eq!(
            event_on_date(svalbard, date(2015, 12, 15), SolarEvent::Sunrise).unwrap(),
            None
        );
        assert!(
            event_on_date(svalbard, date(2015, 12, 15), SolarEvent::Noon)
                .unwrap()
                .is_some()
        );
        assert!(
            event_on_date(svalbard, date(2015, 6, 15), SolarEvent::Midnight)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn invalid_observer_is_an_error() {
        let invalid = observer(f64::NAN, 0.0, 0.0);
        assert!(matches!(
            event_on_date(invalid, date(2024, 1, 1), SolarEvent::Sunrise),
            Err(SolarError::InvalidLatitude(_))
        ));
    }
}
