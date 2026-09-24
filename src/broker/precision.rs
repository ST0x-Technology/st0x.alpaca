//! Alpaca decimal-precision limits for order quantities.

use alloy_primitives::U256;
use rain_math_float::{Float, FloatError};

/// Alpaca supports a maximum of 9 decimal places for order quantities.
///
/// Public because it is part of the broker contract consumers validate
/// against (the CLI rejects over-precise manual quantities before
/// submission rather than silently truncating them).
pub const ALPACA_MAX_DECIMAL_PLACES: u8 = 9;

/// Truncates a Float to at most `max_decimals` decimal places.
///
/// Truncation (floor) is used rather than rounding because rounding up could
/// cause an order for more shares than we actually have.
///
/// Returns `Ok(None)` when truncation would collapse a non-zero value to zero,
/// indicating the value is below the precision threshold and should be
/// preserved in inventory rather than submitted to the broker.
///
/// # Errors
///
/// Returns [`FloatError`] when the fixed-decimal conversion fails.
pub fn truncate_to_decimal_places(
    value: Float,
    max_decimals: u8,
) -> Result<Option<Float>, FloatError> {
    let (fixed, lossless) = value.to_fixed_decimal_lossy(max_decimals)?;

    if lossless {
        return Ok(Some(value));
    }

    let is_nonzero = !value.is_zero()?;
    let truncated_is_zero = fixed == U256::ZERO;

    if is_nonzero && truncated_is_zero {
        return Ok(None);
    }

    Float::from_fixed_decimal(fixed, max_decimals).map(Some)
}

#[cfg(test)]
mod tests {
    use st0x_float_macro::float;

    use super::*;

    #[test]
    fn truncate_whole_number_unchanged() {
        let value = float!(100);
        let result = truncate_to_decimal_places(value, 9).unwrap().unwrap();
        assert!(
            result.eq(value).unwrap(),
            "expected {}, got {}",
            value.format().unwrap(),
            result.format().unwrap(),
        );
    }

    #[test]
    fn truncate_fewer_decimals_unchanged() {
        let value = float!(1.5);
        let result = truncate_to_decimal_places(value, 9).unwrap().unwrap();
        assert!(
            result.eq(value).unwrap(),
            "expected {}, got {}",
            value.format().unwrap(),
            result.format().unwrap(),
        );

        let value = float!(0.123456789);
        let result = truncate_to_decimal_places(value, 9).unwrap().unwrap();
        assert!(
            result.eq(value).unwrap(),
            "expected {}, got {}",
            value.format().unwrap(),
            result.format().unwrap(),
        );
    }

    #[test]
    fn truncate_excess_decimals_floors() {
        let value = Float::parse("0.996350331351928059".to_string()).unwrap();
        let expected = float!(0.996350331);
        let result = truncate_to_decimal_places(value, 9).unwrap().unwrap();
        assert!(
            result.eq(expected).unwrap(),
            "expected {}, got {}",
            expected.format().unwrap(),
            result.format().unwrap(),
        );
    }

    #[test]
    fn truncate_preserves_whole_part() {
        let value = Float::parse("1.500000000000000001".to_string()).unwrap();
        let expected = float!(1.5);
        let result = truncate_to_decimal_places(value, 9).unwrap().unwrap();
        assert!(
            result.eq(expected).unwrap(),
            "expected {}, got {}",
            expected.format().unwrap(),
            result.format().unwrap(),
        );
    }

    #[test]
    fn truncate_integer_value_with_excess_decimals() {
        let value = Float::parse("2.000000000000000001".to_string()).unwrap();
        let expected = float!(2);
        let result = truncate_to_decimal_places(value, 9).unwrap().unwrap();
        assert!(
            result.eq(expected).unwrap(),
            "expected {}, got {}",
            expected.format().unwrap(),
            result.format().unwrap(),
        );
    }

    #[test]
    fn truncate_zero_decimal_places() {
        let value = float!(1.234);
        let expected = float!(1);
        let result = truncate_to_decimal_places(value, 0).unwrap().unwrap();
        assert!(
            result.eq(expected).unwrap(),
            "expected {}, got {}",
            expected.format().unwrap(),
            result.format().unwrap(),
        );
    }

    #[test]
    fn truncate_sub_precision_value_returns_none() {
        let value = float!(0.0000000009);
        assert!(
            truncate_to_decimal_places(value, 9).unwrap().is_none(),
            "sub-precision value {} should return None",
            value.format().unwrap(),
        );
    }

    #[test]
    fn truncate_exact_zero_returns_some() {
        let result = truncate_to_decimal_places(float!(0), 9).unwrap().unwrap();
        assert!(
            result.is_zero().unwrap(),
            "expected zero, got {}",
            result.format().unwrap(),
        );
    }
}
