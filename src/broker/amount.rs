//! Crypto amounts received from Alpaca API responses.

use rain_math_float::{Float, FloatError};
use serde::Deserialize;
use std::fmt::{Debug, Display};

use st0x_finance::{HasZero, Usdc, UsdcConversionError};

/// A crypto amount normalized at the Alpaca response boundary.
///
/// Alpaca reports crypto quantities with up to nine decimals while on-chain
/// USDC has six. Construction always floors onto that grid, and only the
/// normalized amount can leave the boundary. The raw amount remains private
/// for exact broker-side cash valuation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AlpacaAmount {
    raw: Usdc,
    normalized: Usdc,
}

impl AlpacaAmount {
    #[must_use]
    pub fn into_normalized(self) -> Float {
        self.normalized.inner()
    }

    /// Whether the normalized amount is zero.
    ///
    /// # Errors
    ///
    /// Returns [`FloatError`] when the zero comparison fails.
    pub fn is_zero(&self) -> Result<bool, FloatError> {
        self.normalized.is_zero()
    }

    /// Cash value of the raw (unnormalized) broker amount at `price`.
    ///
    /// # Errors
    ///
    /// Returns [`FloatError`] when the multiplication fails.
    pub fn cash_value_at(self, price: Float) -> Result<Usdc, FloatError> {
        self.raw * price
    }
}

impl TryFrom<Float> for AlpacaAmount {
    type Error = UsdcConversionError;

    fn try_from(amount: Float) -> Result<Self, Self::Error> {
        let raw = Usdc::new(amount);
        let normalized = floor_to_6_decimals(raw)?;

        Ok(Self { raw, normalized })
    }
}

/// Floors to USDC's 6-decimal on-chain grid, truncating any finer precision
/// Alpaca reports (up to 9 decimals). Never rounds up, so the result never
/// exceeds what the broker holds.
fn floor_to_6_decimals(amount: Usdc) -> Result<Usdc, UsdcConversionError> {
    if amount
        .inner()
        .lt(Float::zero()?)
        .map_err(UsdcConversionError::Float)?
    {
        return Err(UsdcConversionError::NegativeValue(amount.inner()));
    }

    let (fixed, _lossless) = amount
        .inner()
        .to_fixed_decimal_lossy(6)
        .map_err(UsdcConversionError::Float)?;

    Float::from_fixed_decimal(fixed, 6)
        .map(Usdc::new)
        .map_err(UsdcConversionError::Float)
}

impl<'de> Deserialize<'de> for AlpacaAmount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let amount = Usdc::deserialize(deserializer).map_err(|error| {
            serde::de::Error::custom(format_args!("Invalid Alpaca crypto Float: {error}"))
        })?;
        Self::try_from(amount.inner()).map_err(serde::de::Error::custom)
    }
}

impl Debug for AlpacaAmount {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("AlpacaAmount")
            .field(&self.normalized)
            .finish()
    }
}

impl Display for AlpacaAmount {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.normalized, formatter)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;
    use proptest::prelude::*;
    use rain_math_float::Float;
    use serde_json::json;

    use st0x_finance::{HasZero, Usdc, UsdcConversionError};
    use st0x_float_macro::float;

    use super::{AlpacaAmount, floor_to_6_decimals};

    #[test]
    fn floor_truncates_nine_decimal_fill_to_six() {
        let usdc = Usdc::new(float!(9794.019706861));
        let floored = floor_to_6_decimals(usdc).unwrap();
        assert_eq!(floored, Usdc::new(float!(9794.019706)));
        assert_eq!(
            floored.to_u256_6_decimals().unwrap(),
            U256::from(9_794_019_706u64)
        );
    }

    #[test]
    fn floor_leaves_six_decimal_amount_unchanged() {
        let usdc = Usdc::new(float!(1000.123456));
        assert_eq!(floor_to_6_decimals(usdc).unwrap(), usdc);
    }

    #[test]
    fn floor_leaves_zero_unchanged() {
        assert_eq!(floor_to_6_decimals(Usdc::ZERO).unwrap(), Usdc::ZERO);
    }

    #[test]
    fn floor_rejects_negative_amount() {
        let usdc = Usdc::new(float!(-1.1234567));
        let error = floor_to_6_decimals(usdc).unwrap_err();
        assert!(matches!(error, UsdcConversionError::NegativeValue(_)));
    }

    proptest! {
        /// Flooring a nonnegative 9-decimal amount never increases it,
        /// always lands on the 6-decimal grid (strict conversion succeeds
        /// and round-trips), and is idempotent.
        #[test]
        fn floor_is_sound_for_nonnegative_amounts(raw in 0u64..=u64::MAX) {
            let amount = Usdc::new(Float::from_fixed_decimal(U256::from(raw), 9).unwrap());

            let floored = floor_to_6_decimals(amount).unwrap();

            prop_assert!(floored <= amount);
            let fixed = floored.to_u256_6_decimals().unwrap();
            prop_assert_eq!(fixed, U256::from(raw / 1_000));
            prop_assert_eq!(
                Usdc::new(Float::from_fixed_decimal(fixed, 6).unwrap()),
                floored
            );
            prop_assert_eq!(floor_to_6_decimals(floored).unwrap(), floored);
        }
    }

    #[test]
    fn string_amount_is_floored_to_six_decimals() {
        let amount: AlpacaAmount = serde_json::from_value(json!("9.794019706")).unwrap();

        assert_eq!(
            Usdc::new(amount.into_normalized()),
            Usdc::new(float!(9.794019))
        );
    }

    #[test]
    fn numeric_amount_is_floored_to_six_decimals() {
        let amount: AlpacaAmount = serde_json::from_value(json!(9.794_019_706)).unwrap();

        assert_eq!(
            Usdc::new(amount.into_normalized()),
            Usdc::new(float!(9.794019))
        );
    }

    #[test]
    fn display_uses_the_normalized_amount() {
        let amount: AlpacaAmount = serde_json::from_value(json!("9.794019706")).unwrap();

        assert_eq!(amount.to_string(), "9.794019");
    }

    #[test]
    fn negative_amount_is_rejected() {
        let error = serde_json::from_value::<AlpacaAmount>(json!("-1.000000001")).unwrap_err();

        assert!(error.to_string().contains("cannot be negative"));
    }

    #[test]
    fn cash_valuation_preserves_the_raw_broker_amount() {
        let amount: AlpacaAmount = serde_json::from_value(json!("9.794019706")).unwrap();

        assert_eq!(
            amount.cash_value_at(float!(1.00101001)).unwrap(),
            Usdc::new(float!(9.80391176384325706))
        );
    }
}
