//! Color formulas copied from the pinned Home Assistant color utility.
//!
//! See `homeassistant/util/color.py` at the commit recorded in the crate root
//! docs. Gamut clipping is omitted because the source's light helpers call
//! these functions without a lamp gamut.

use super::{LightError, round_ties_even};

/// Convert hue in degrees and saturation in percent to full-value RGB.
///
/// This follows `color_hs_to_RGB` and `color_hsv_to_RGB` in Home Assistant's
/// pinned `homeassistant/util/color.py`.
#[must_use]
pub fn hs_to_rgb(hue: f64, saturation: f64) -> [u8; 3] {
    hsv_to_rgb(hue, saturation, 100.0)
}

fn hsv_to_rgb(hue: f64, saturation: f64, value: f64) -> [u8; 3] {
    let mut hue = (hue / 360.0) % 1.0;
    if hue < 0.0 {
        hue += 1.0;
    }
    let saturation = saturation / 100.0;
    let value = value / 100.0;
    if saturation == 0.0 {
        let gray = round_ties_even(value * 255.0).clamp(0.0, 255.0) as u8;
        return [gray; 3];
    }
    let index = (hue * 6.0).floor() as i32;
    let fraction = hue * 6.0 - f64::from(index);
    let p = value * (1.0 - saturation);
    let q = value * (1.0 - saturation * fraction);
    let t = value * (1.0 - saturation * (1.0 - fraction));
    let (red, green, blue) = match index.rem_euclid(6) {
        0 => (value, t, p),
        1 => (q, value, p),
        2 => (p, value, t),
        3 => (p, q, value),
        4 => (t, p, value),
        _ => (value, p, q),
    };
    [red, green, blue].map(|channel| round_ties_even(channel * 255.0).clamp(0.0, 255.0) as u8)
}

/// Convert RGB to hue in degrees and saturation in percent.
///
/// The output rounds each channel to three decimals as
/// `color_RGB_to_hsv` does in the pinned Home Assistant source.
#[must_use]
pub fn rgb_to_hs(rgb: [u8; 3]) -> [f64; 2] {
    rgb_f64_to_hs(rgb.map(f64::from))
}

fn rgb_f64_to_hs(rgb: [f64; 3]) -> [f64; 2] {
    let [red, green, blue] = rgb.map(|channel| channel / 255.0);
    let maximum = red.max(green).max(blue);
    let minimum = red.min(green).min(blue);
    let delta = maximum - minimum;
    let hue = if delta == 0.0 {
        0.0
    } else if maximum == red {
        ((green - blue) / delta).rem_euclid(6.0) / 6.0
    } else if maximum == green {
        ((blue - red) / delta + 2.0) / 6.0
    } else {
        ((red - green) / delta + 4.0) / 6.0
    };
    let saturation = if maximum == 0.0 { 0.0 } else { delta / maximum };
    [round_to(hue * 360.0, 3), round_to(saturation * 100.0, 3)]
}

/// Convert RGB to CIE xy using Home Assistant's wide RGB D65 matrix.
#[must_use]
pub fn rgb_to_xy(rgb: [u8; 3]) -> [f64; 2] {
    if rgb.iter().all(|channel| *channel == 0) {
        return [0.0, 0.0];
    }
    let [mut red, mut green, mut blue] = rgb.map(|channel| f64::from(channel) / 255.0);
    red = gamma_correct(red);
    green = gamma_correct(green);
    blue = gamma_correct(blue);
    let x_value = red * 0.664_511 + green * 0.154_324 + blue * 0.162_028;
    let y_value = red * 0.283_881 + green * 0.668_433 + blue * 0.047_685;
    let z_value = red * 0.000_088 + green * 0.072_310 + blue * 0.986_039;
    let total = x_value + y_value + z_value;
    [round_to(x_value / total, 3), round_to(y_value / total, 3)]
}

/// Convert CIE xy to full-brightness RGB using Home Assistant's wide RGB D65
/// inverse matrix. No gamut clipping is applied.
#[must_use]
pub fn xy_to_rgb(xy: [f64; 2]) -> [u8; 3] {
    let [x, y] = xy;
    let brightness = 1.0;
    let y = y.max(1e-11);
    let x_value = (brightness / y) * x;
    let z_value = (brightness / y) * (1.0 - x - y);
    let red = x_value * 1.656_492 - brightness * 0.354_851 - z_value * 0.255_038;
    let green = -x_value * 0.707_196 + brightness * 1.655_397 + z_value * 0.036_152;
    let blue = x_value * 0.051_713 - brightness * 0.121_364 + z_value * 1.011_530;
    let mut channels = [red, green, blue].map(reverse_gamma_correct);
    channels
        .iter_mut()
        .for_each(|channel| *channel = channel.max(0.0));
    let maximum = channels[0].max(channels[1]).max(channels[2]);
    if maximum > 1.0 {
        channels.iter_mut().for_each(|channel| *channel /= maximum);
    }
    channels.map(|channel| (channel * 255.0).trunc().clamp(0.0, 255.0) as u8)
}

/// Convert RGB to RGBW by extracting the shared white channel and matching
/// the output maximum to the input maximum.
#[must_use]
pub fn rgb_to_rgbw(rgb: [u8; 3]) -> [u8; 4] {
    let white = *rgb.iter().min().expect("RGB has three channels");
    scale_to_input_max(
        rgb.to_vec(),
        [
            f64::from(rgb[0] - white),
            f64::from(rgb[1] - white),
            f64::from(rgb[2] - white),
            f64::from(white),
        ],
    )
}

/// Convert RGBW to RGB, adding the white channel and matching the output
/// maximum to the input maximum.
#[must_use]
pub fn rgbw_to_rgb(rgbw: [u8; 4]) -> [u8; 3] {
    let rgb = [
        u16::from(rgbw[0]) + u16::from(rgbw[3]),
        u16::from(rgbw[1]) + u16::from(rgbw[3]),
        u16::from(rgbw[2]) + u16::from(rgbw[3]),
    ];
    scale_to_input_max(rgbw.into_iter().collect(), rgb.map(f64::from))
}

/// Convert RGB to RGBWW using Home Assistant's midpoint-white extraction
/// formula and a warm/cold Kelvin range.
pub fn rgb_to_rgbww(rgb: [u8; 3], min_kelvin: u32, max_kelvin: u32) -> Result<[u8; 5], LightError> {
    validate_kelvin_range(min_kelvin, max_kelvin)?;
    let max_mired = kelvin_to_mired(min_kelvin);
    let min_mired = kelvin_to_mired(max_kelvin);
    let mired_range = max_mired - min_mired;
    let midpoint_mired = min_mired + mired_range / 2.0;
    let midpoint_kelvin = mired_to_kelvin(midpoint_mired)?;
    let [white_red, white_green, white_blue] = kelvin_to_rgb(midpoint_kelvin);
    let white_level = [
        if white_red == 0.0 {
            0.0
        } else {
            f64::from(rgb[0]) / white_red
        },
        if white_green == 0.0 {
            0.0
        } else {
            f64::from(rgb[1]) / white_green
        },
        if white_blue == 0.0 {
            0.0
        } else {
            f64::from(rgb[2]) / white_blue
        },
    ]
    .into_iter()
    .fold(f64::INFINITY, f64::min);
    let rgbww = [
        f64::from(rgb[0]) - white_red * white_level,
        f64::from(rgb[1]) - white_green * white_level,
        f64::from(rgb[2]) - white_blue * white_level,
        round_ties_even(white_level * 255.0),
        round_ties_even(white_level * 255.0),
    ];
    Ok(scale_to_input_max(rgb.into_iter().collect(), rgbww))
}

/// Convert RGBWW to RGB using the color temperature represented by the two
/// white channels and Home Assistant's output rescaling formula.
pub fn rgbww_to_rgb(
    rgbww: [u8; 5],
    min_kelvin: u32,
    max_kelvin: u32,
) -> Result<[u8; 3], LightError> {
    validate_kelvin_range(min_kelvin, max_kelvin)?;
    let max_mired = kelvin_to_mired(min_kelvin);
    let min_mired = kelvin_to_mired(max_kelvin);
    let white_total = u16::from(rgbww[3]) + u16::from(rgbww[4]);
    let ratio = if white_total == 0 {
        0.5
    } else {
        f64::from(rgbww[4]) / f64::from(white_total)
    };
    let color_mired = min_mired + ratio * (max_mired - min_mired);
    let temperature = if color_mired == 0.0 {
        0
    } else {
        mired_to_kelvin(color_mired)?
    };
    let [white_red, white_green, white_blue] = kelvin_to_rgb(temperature);
    let white_level = f64::from(rgbww[3].max(rgbww[4])) / 255.0;
    let rgb = [
        f64::from(rgbww[0]) + white_red * white_level,
        f64::from(rgbww[1]) + white_green * white_level,
        f64::from(rgbww[2]) + white_blue * white_level,
    ];
    Ok(scale_to_input_max(rgbww.into_iter().collect(), rgb))
}

/// Approximate an RGB color for a color temperature in Kelvin using the
/// pinned T. Helland formula from Home Assistant.
#[must_use]
pub fn kelvin_to_rgb(kelvin: u32) -> [f64; 3] {
    let temperature = f64::from(kelvin.clamp(1_000, 40_000)) / 100.0;
    let red = if temperature <= 66.0 {
        255.0
    } else {
        329.698_727_446 * (temperature - 60.0).powf(-0.133_204_759_2)
    };
    let green = if temperature <= 66.0 {
        99.470_802_586_1 * temperature.ln() - 161.119_568_166_1
    } else {
        288.122_169_528_3 * (temperature - 60.0).powf(-0.075_514_849_2)
    };
    let blue = if temperature >= 66.0 {
        255.0
    } else if temperature <= 19.0 {
        0.0
    } else {
        138.517_731_223_1 * (temperature - 10.0).ln() - 305.044_792_730_7
    };
    [red, green, blue].map(|channel| channel.clamp(0.0, 255.0))
}

/// Return a color temperature's hue and saturation using Home Assistant's
/// approximate Kelvin-to-RGB conversion.
pub(crate) fn kelvin_to_hs(kelvin: u32) -> [f64; 2] {
    rgb_f64_to_hs(kelvin_to_rgb(kelvin))
}

/// Convert a color temperature and brightness to RGBWW white-channel values.
pub fn kelvin_to_rgbww(
    temperature: u32,
    brightness: u8,
    min_kelvin: u32,
    max_kelvin: u32,
) -> Result<[u8; 5], LightError> {
    validate_kelvin_range(min_kelvin, max_kelvin)?;
    let maximum_mired = kelvin_to_mired(min_kelvin);
    let minimum_mired = kelvin_to_mired(max_kelvin);
    let temperature_mired = kelvin_to_mired(temperature.clamp(min_kelvin, max_kelvin));
    let range = maximum_mired - minimum_mired;
    if range == 0.0 {
        return Ok([0, 0, 0, 0, brightness]);
    }
    let cold = ((maximum_mired - temperature_mired) / range) * f64::from(brightness);
    let warm = f64::from(brightness) - cold;
    Ok([
        0,
        0,
        0,
        round_ties_even(cold).clamp(0.0, 255.0) as u8,
        round_ties_even(warm).clamp(0.0, 255.0) as u8,
    ])
}

/// Convert CIE xy to Kelvin using Home Assistant's McCamy approximation.
/// Returns an error for its singular denominator at `y == 0.1858`.
pub fn xy_to_kelvin(xy: [f64; 2]) -> Result<u32, LightError> {
    let [x, y] = xy;
    if !x.is_finite() || !y.is_finite() {
        return Err(LightError::InvalidColorConversion);
    }
    let denominator = 0.1858 - y;
    if denominator == 0.0 {
        return Err(LightError::InvalidColorConversion);
    }
    let n = (x - 0.3320) / denominator;
    let cct = 437.0 * n.powi(3) + 3_601.0 * n.powi(2) + 6_861.0 * n + 5_517.0;
    if !cct.is_finite() {
        return Err(LightError::InvalidColorConversion);
    }
    Ok(cct.trunc().clamp(0.0, f64::from(u32::MAX)) as u32)
}

fn gamma_correct(channel: f64) -> f64 {
    if channel > 0.04045 {
        ((channel + 0.055) / 1.055).powf(2.4)
    } else {
        channel / 12.92
    }
}

fn reverse_gamma_correct(channel: f64) -> f64 {
    if channel <= 0.003_130_8 {
        12.92 * channel
    } else {
        1.055 * channel.powf(1.0 / 2.4) - 0.055
    }
}

fn round_to(value: f64, places: u32) -> f64 {
    let scale = 10_f64.powi(places as i32);
    round_ties_even(value * scale) / scale
}

fn scale_to_input_max<const N: usize>(input: Vec<u8>, output: [f64; N]) -> [u8; N] {
    let maximum_input = input.into_iter().max().unwrap_or_default();
    let maximum_output = output.iter().copied().fold(0.0_f64, f64::max);
    let factor = if maximum_output == 0.0 {
        0.0
    } else {
        f64::from(maximum_input) / maximum_output
    };
    output.map(|channel| round_ties_even(channel * factor).clamp(0.0, 255.0).round() as u8)
}

fn kelvin_to_mired(kelvin: u32) -> f64 {
    (1_000_000.0 / f64::from(kelvin)).floor()
}

fn mired_to_kelvin(mired: f64) -> Result<u32, LightError> {
    if mired <= 0.0 || !mired.is_finite() {
        return Err(LightError::InvalidTemperatureRange);
    }
    Ok((1_000_000.0 / mired)
        .floor()
        .clamp(0.0, f64::from(u32::MAX)) as u32)
}

fn validate_kelvin_range(min_kelvin: u32, max_kelvin: u32) -> Result<(), LightError> {
    if min_kelvin == 0 || max_kelvin == 0 || min_kelvin > max_kelvin {
        return Err(LightError::InvalidTemperatureRange);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_math_golden_values_cover_rgb_hs_xy_and_white_channels() {
        assert_eq!(hs_to_rgb(0.0, 100.0), [255, 0, 0]);
        assert_eq!(hs_to_rgb(120.0, 100.0), [0, 255, 0]);
        assert_eq!(rgb_to_hs([255, 0, 0]), [0.0, 100.0]);
        assert_eq!(rgb_to_xy([255, 0, 0]), [0.701, 0.299]);
        assert_eq!(rgb_to_xy([255, 255, 255]), [0.323, 0.329]);
        assert_eq!(xy_to_rgb([0.701, 0.299]), [255, 0, 0]);
        assert_eq!(rgb_to_rgbw([255, 255, 255]), [0, 0, 0, 255]);
        assert_eq!(rgbw_to_rgb([0, 0, 0, 255]), [255, 255, 255]);
    }

    #[test]
    fn source_math_golden_values_cover_rgbww_temperature_endpoints() {
        let rgbww = rgb_to_rgbww([255, 255, 255], 2_000, 6_500).unwrap();
        assert_eq!(rgbww, [0, 76, 141, 255, 255]);
        assert_eq!(rgbww_to_rgb(rgbww, 2_000, 6_500).unwrap(), [255, 255, 255]);
        assert_eq!(
            kelvin_to_rgbww(2_000, 127, 2_000, 6_500).unwrap(),
            [0, 0, 0, 0, 127]
        );
        assert_eq!(
            kelvin_to_rgbww(6_500, 127, 2_000, 6_500).unwrap(),
            [0, 0, 0, 127, 0]
        );
    }

    #[test]
    fn mccamy_temperature_singularity_is_explicit() {
        assert_eq!(
            xy_to_kelvin([0.3320, 0.1858]),
            Err(LightError::InvalidColorConversion)
        );
        assert_eq!(xy_to_kelvin([0.3320, 0.1857]).unwrap(), 5_517);
    }
}
