use std::collections::HashMap;

use billing::model::ModelUsage;
use billing::ports::Pricing;
use kernel::model::{ModelId, Money};

use crate::pricing::{ModelPrice, PricingError};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DuplicateModel(pub ModelId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PriceLookupError {
    UnknownModel,
    Overflow,
}

/// 按 `ModelId` 精确查价，区分大小写，不做前缀或别名回退。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceBook {
    prices: HashMap<ModelId, ModelPrice>,
}

impl PriceBook {
    /// # Errors
    ///
    /// 同一个 `ModelId` 出现两次时返回 `DuplicateModel`。
    pub fn new(
        entries: impl IntoIterator<Item = (ModelId, ModelPrice)>,
    ) -> Result<Self, DuplicateModel> {
        let mut prices = HashMap::new();
        for (model, price) in entries {
            if prices.contains_key(&model) {
                return Err(DuplicateModel(model));
            }
            prices.insert(model, price);
        }
        Ok(Self { prices })
    }

    #[must_use]
    pub fn get(&self, model: &ModelId) -> Option<&ModelPrice> {
        self.prices.get(model)
    }
}

impl Pricing for PriceBook {
    type Usage = ModelUsage;
    type Error = PriceLookupError;

    fn price(&self, usage: &ModelUsage) -> Result<Money, PriceLookupError> {
        let price = self
            .get(&usage.model)
            .ok_or(PriceLookupError::UnknownModel)?;
        price.price(&usage.tokens).map_err(|error| match error {
            PricingError::Overflow => PriceLookupError::Overflow,
        })
    }
}

#[cfg(test)]
mod tests {
    use billing::model::TokenUsage;
    use kernel::model::Currency;

    use super::*;
    use crate::pricing::NanosPerMillionTokens;

    fn model(name: &str) -> ModelId {
        ModelId(String::from(name))
    }

    fn price(input: u64, cached_input: u64, output: u64) -> ModelPrice {
        match ModelPrice::new(
            Currency::Usd,
            NanosPerMillionTokens::new(input),
            NanosPerMillionTokens::new(cached_input),
            NanosPerMillionTokens::new(output),
        ) {
            Ok(price) => price,
            Err(error) => panic!("{error:?}"),
        }
    }

    fn book() -> PriceBook {
        match PriceBook::new([
            (
                model("gpt-4o"),
                price(2_500_000_000, 1_250_000_000, 10_000_000_000),
            ),
            (
                model("gpt-4o-mini"),
                price(150_000_000, 75_000_000, 600_000_000),
            ),
        ]) {
            Ok(book) => book,
            Err(error) => panic!("{error:?}"),
        }
    }

    fn usage(
        name: &str,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
    ) -> ModelUsage {
        ModelUsage {
            model: model(name),
            tokens: TokenUsage {
                input_tokens,
                cached_input_tokens,
                output_tokens,
            },
        }
    }

    fn usd(minor_units: u64) -> Money {
        Money {
            currency: Currency::Usd,
            minor_units,
        }
    }

    #[test]
    fn prices_the_named_model() {
        let book = book();
        assert_eq!(
            book.price(&usage("gpt-4o-mini", 19, 0, 816)),
            Ok(usd(492_450))
        );
        assert_eq!(book.price(&usage("gpt-4o", 1, 0, 1)), Ok(usd(12_500)));
    }

    #[test]
    fn matches_the_pure_model_price() {
        let book = book();
        let request = usage("gpt-4o-mini", 40, 60, 7);
        let direct = price(150_000_000, 75_000_000, 600_000_000).price(&request.tokens);
        assert_eq!(
            book.price(&request),
            direct.map_err(|_| PriceLookupError::Overflow)
        );
    }

    #[test]
    fn unknown_model_is_rejected() {
        let book = book();
        assert_eq!(
            book.price(&usage("gpt-5", 1, 0, 1)),
            Err(PriceLookupError::UnknownModel)
        );
    }

    #[test]
    fn match_is_exact_and_case_sensitive() {
        let book = book();
        assert_eq!(
            book.price(&usage("GPT-4o-mini", 1, 0, 1)),
            Err(PriceLookupError::UnknownModel)
        );
        assert_eq!(
            book.price(&usage("gpt-4o-2024-11-20", 1, 0, 1)),
            Err(PriceLookupError::UnknownModel)
        );
        assert_eq!(
            book.price(&usage("gpt-4", 1, 0, 1)),
            Err(PriceLookupError::UnknownModel)
        );
    }

    #[test]
    fn overflow_is_reported() {
        let book = book();
        assert_eq!(
            book.price(&usage("gpt-4o", u64::MAX, 0, 0)),
            Err(PriceLookupError::Overflow)
        );
    }

    #[test]
    fn duplicate_model_is_rejected() {
        let result = PriceBook::new([
            (model("gpt-4o"), price(2, 1, 2)),
            (model("gpt-4o"), price(3, 1, 3)),
        ]);
        assert_eq!(result, Err(DuplicateModel(model("gpt-4o"))));
    }

    #[test]
    fn get_returns_the_stored_price() {
        let book = book();
        assert_eq!(
            book.get(&model("gpt-4o-mini")),
            Some(&price(150_000_000, 75_000_000, 600_000_000))
        );
        assert_eq!(book.get(&model("gpt-4o-mini ")), None);
    }

    #[test]
    fn rate_round_trips() {
        assert_eq!(NanosPerMillionTokens::new(7).get(), 7);
    }
}
