use billing::model::TokenUsage;
use billing::ports::Pricing;
use kernel::model::{Currency, Money};

pub const TOKENS_PER_PRICE_UNIT: u64 = 1_000_000;

/// 每百万 token 的价格，单位是 `Money::minor_units`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NanosPerMillionTokens(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PriceRejection {
    UnsupportedCurrency,
    ZeroRate,
    CachedInputAboveInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PricingError {
    Overflow,
}

/// 一个模型的单价。金额是三项用量乘单价之和除以 `TOKENS_PER_PRICE_UNIT`，整笔只向上取整一次。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModelPrice {
    currency: Currency,
    input: NanosPerMillionTokens,
    cached_input: NanosPerMillionTokens,
    output: NanosPerMillionTokens,
}

impl ModelPrice {
    /// # Errors
    ///
    /// `currency` 不是 `Currency::Usd` 时返回 `PriceRejection::UnsupportedCurrency`。任一单价为 0 时返回 `PriceRejection::ZeroRate`。`cached_input` 大于 `input` 时返回 `PriceRejection::CachedInputAboveInput`。
    pub fn new(
        currency: Currency,
        input: NanosPerMillionTokens,
        cached_input: NanosPerMillionTokens,
        output: NanosPerMillionTokens,
    ) -> Result<Self, PriceRejection> {
        if currency != Currency::Usd {
            return Err(PriceRejection::UnsupportedCurrency);
        }
        if input.0 == 0 || cached_input.0 == 0 || output.0 == 0 {
            return Err(PriceRejection::ZeroRate);
        }
        if cached_input > input {
            return Err(PriceRejection::CachedInputAboveInput);
        }
        Ok(Self {
            currency,
            input,
            cached_input,
            output,
        })
    }
}

impl Pricing for ModelPrice {
    type Usage = TokenUsage;
    type Error = PricingError;

    fn price(&self, usage: &TokenUsage) -> Result<Money, PricingError> {
        let total = cost(usage.input_tokens, self.input)
            .checked_add(cost(usage.cached_input_tokens, self.cached_input))
            .and_then(|sum| sum.checked_add(cost(usage.output_tokens, self.output)))
            .ok_or(PricingError::Overflow)?;
        let minor_units = u64::try_from(total.div_ceil(u128::from(TOKENS_PER_PRICE_UNIT)))
            .map_err(|_| PricingError::Overflow)?;
        Ok(Money {
            currency: self.currency,
            minor_units,
        })
    }
}

fn cost(tokens: u64, rate: NanosPerMillionTokens) -> u128 {
    u128::from(tokens) * u128::from(rate.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(value: u64) -> NanosPerMillionTokens {
        NanosPerMillionTokens(value)
    }

    fn usd(minor_units: u64) -> Money {
        Money {
            currency: Currency::Usd,
            minor_units,
        }
    }

    fn usage(input_tokens: u64, cached_input_tokens: u64, output_tokens: u64) -> TokenUsage {
        TokenUsage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
        }
    }

    fn price_of(input: u64, cached_input: u64, output: u64) -> Result<ModelPrice, PriceRejection> {
        ModelPrice::new(Currency::Usd, rate(input), rate(cached_input), rate(output))
    }

    fn gpt_4o_mini() -> Result<ModelPrice, PriceRejection> {
        price_of(150_000_000, 75_000_000, 600_000_000)
    }

    #[test]
    fn known_price_is_exact() {
        let price = gpt_4o_mini();
        assert_eq!(
            price.map(|p| p.price(&usage(19, 0, 816))),
            Ok(Ok(usd(492_450)))
        );
    }

    #[test]
    fn cached_input_is_priced_once_at_the_cached_rate() {
        let price = gpt_4o_mini();
        assert_eq!(
            price.map(|p| p.price(&usage(40, 60, 0))),
            Ok(Ok(usd(10_500)))
        );
    }

    #[test]
    fn output_tokens_include_reasoning_at_the_output_rate() {
        let price = gpt_4o_mini();
        assert_eq!(
            price.map(|p| p.price(&usage(0, 0, 1_000))),
            Ok(Ok(usd(600_000)))
        );
    }

    #[test]
    fn total_rounds_up_once() {
        let price = price_of(600_000, 300_000, 700_000);
        assert_eq!(price.map(|p| p.price(&usage(1, 1, 1))), Ok(Ok(usd(2))));
        assert_eq!(price.map(|p| p.price(&usage(1, 0, 0))), Ok(Ok(usd(1))));
    }

    #[test]
    fn exact_multiple_is_not_rounded_up() {
        let price = price_of(1_000_000, 500_000, 2_000_000);
        assert_eq!(price.map(|p| p.price(&usage(3, 2, 1))), Ok(Ok(usd(6))));
    }

    #[test]
    fn zero_usage_is_zero() {
        let price = gpt_4o_mini();
        assert_eq!(price.map(|p| p.price(&usage(0, 0, 0))), Ok(Ok(usd(0))));
    }

    #[test]
    fn sum_overflowing_u128_is_an_error() {
        let price = price_of(u64::MAX, u64::MAX, u64::MAX);
        assert_eq!(
            price.map(|p| p.price(&usage(u64::MAX, u64::MAX, 0))),
            Ok(Err(PricingError::Overflow))
        );
        assert_eq!(
            price.map(|p| p.price(&usage(u64::MAX, 0, u64::MAX))),
            Ok(Err(PricingError::Overflow))
        );
    }

    #[test]
    fn amount_above_u64_is_an_error() {
        let price = price_of(2_000_000, 1, 1);
        assert_eq!(
            price.map(|p| p.price(&usage(u64::MAX, 0, 0))),
            Ok(Err(PricingError::Overflow))
        );
    }

    #[test]
    fn largest_amount_fits() {
        let price = price_of(1_000_000, 1, 1);
        assert_eq!(
            price.map(|p| p.price(&usage(u64::MAX, 0, 0))),
            Ok(Ok(usd(u64::MAX)))
        );
    }

    #[test]
    fn zero_rates_are_rejected() {
        assert_eq!(price_of(0, 1, 1), Err(PriceRejection::ZeroRate));
        assert_eq!(price_of(1, 0, 1), Err(PriceRejection::ZeroRate));
        assert_eq!(price_of(1, 1, 0), Err(PriceRejection::ZeroRate));
    }

    #[test]
    fn cached_rate_above_input_rate_is_rejected() {
        assert_eq!(
            price_of(2, 3, 1),
            Err(PriceRejection::CachedInputAboveInput)
        );
        assert!(price_of(2, 2, 1).is_ok());
    }

    #[test]
    fn only_usd_is_accepted() {
        assert_eq!(
            ModelPrice::new(Currency::Cny, rate(1), rate(1), rate(1)),
            Err(PriceRejection::UnsupportedCurrency)
        );
    }

    #[test]
    fn hold_estimate_covers_every_smaller_usage() {
        let Ok(price) = price_of(333_333, 111_111, 777_777) else {
            panic!("valid price");
        };
        for estimated_input in 0..=6 {
            for estimated_output in 0..=6 {
                let hold = price.price(&usage(estimated_input, 0, estimated_output));
                for input in 0..=estimated_input {
                    for cached in 0..=estimated_input - input {
                        for output in 0..=estimated_output {
                            let settle = price.price(&usage(input, cached, output));
                            assert!(
                                matches!((settle, hold), (Ok(s), Ok(h)) if s.minor_units <= h.minor_units)
                            );
                        }
                    }
                }
            }
        }
    }
}
