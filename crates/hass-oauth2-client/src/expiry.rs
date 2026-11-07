//! Python-compatible integer coercion for valid provider expiry values.

use serde_json::Value;
use std::sync::OnceLock;

const DECIMAL_ZEROES: &str = include_str!("../data/unicode16-decimal-zeroes.txt");

pub(crate) fn seconds(value: &Value) -> Option<i64> {
    let result = match value {
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                integer
            } else {
                let value = number.as_f64()?.trunc();
                if !value.is_finite() || value <= 0.0 || value >= i64::MAX as f64 {
                    return None;
                }
                value as i64
            }
        }
        Value::String(string) => {
            let string = string.trim_matches(char::is_whitespace);
            let digits = string.strip_prefix('+').unwrap_or(string);
            let mut value = 0_i64;
            let mut count = 0;
            let mut previous_digit = false;
            for character in digits.chars() {
                if character == '_' {
                    if !previous_digit {
                        return None;
                    }
                    previous_digit = false;
                } else {
                    let digit = decimal_digit(character)?;
                    count += 1;
                    if count > 4_300 {
                        return None;
                    }
                    value = value.checked_mul(10)?.checked_add(i64::from(digit))?;
                    previous_digit = true;
                }
            }
            if !previous_digit {
                return None;
            }
            value
        }
        // Bool is intentionally rejected rather than silently becoming 0 or 1.
        _ => return None,
    };
    (result > 0).then_some(result)
}

fn decimal_digit(character: char) -> Option<u32> {
    static ZEROES: OnceLock<Vec<u32>> = OnceLock::new();
    let codepoint = character as u32;
    ZEROES
        .get_or_init(|| {
            DECIMAL_ZEROES
                .lines()
                .filter_map(|line| u32::from_str_radix(line, 16).ok())
                .collect()
        })
        .iter()
        .find_map(|zero| codepoint.checked_sub(*zero).filter(|digit| *digit < 10))
}

#[cfg(test)]
mod tests {
    use super::seconds;
    use serde_json::json;

    #[test]
    fn coerces_source_integer_strings_unicode_digits_and_finite_floats() {
        for value in [
            json!(3600),
            json!(3600.9),
            json!("3600"),
            json!(" +3_600 "),
            json!("٣_٦٠٠"),
            json!("\u{2000}3600\u{2000}"),
            json!("\u{1ccf3}\u{1ccf6}\u{1ccf0}\u{1ccf0}"),
        ] {
            assert_eq!(seconds(&value), Some(3600));
        }
    }

    #[test]
    fn rejects_invalid_coercions_zero_negative_and_overflow() {
        for value in [
            json!(null),
            json!(true),
            json!(0),
            json!(-1),
            json!(0.9),
            json!("1.5"),
            json!("1__2"),
            json!("1_"),
            json!("_1"),
            json!("-1"),
            json!("1e3"),
            json!("\u{1c}1"),
            json!(""),
            json!("+"),
            json!(9223372036854775808_u64),
        ] {
            assert_eq!(seconds(&value), None, "{value:?}");
        }
        assert_eq!(seconds(&json!(format!("{}1", "0".repeat(4_300)))), None);
    }
}
