//! Model pricing → USD cost from token [`Usage`].
//!
//! Token counts are only half of "what did this run cost" — the other half is a
//! price. This module turns a [`Usage`] plus a model id into dollars. It ships a
//! [`PriceTable::builtin`] with the Claude family and DeepSeek published rates,
//! matched by longest-prefix so `claude-opus-4-8-20990101` still resolves. The
//! table is user-owned: [`PriceTable::insert`] overrides or adds a model (e.g. a
//! private alias), and an unknown model returns `None` — cost is reported as
//! "unknown", never a fabricated number.
//!
//! Prices are USD per 1,000,000 tokens. Cached-read and cache-write are billed
//! separately from fresh input, matching how the providers price prompt caching.

use std::collections::HashMap;

use crate::Usage;

/// Per-million-token prices (USD) for one model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    pub input_per_m: f64,
    /// Reading a cached prompt prefix (a fraction of `input_per_m`).
    pub cache_read_per_m: f64,
    /// Writing a prompt prefix into the cache (a premium over `input_per_m`).
    pub cache_write_per_m: f64,
    pub output_per_m: f64,
}

impl ModelPrice {
    /// A price with no prompt-cache tiers — cached reads/writes bill at the input
    /// rate. Useful for providers that don't price caching separately.
    pub const fn flat(input_per_m: f64, output_per_m: f64) -> Self {
        ModelPrice {
            input_per_m,
            cache_read_per_m: input_per_m,
            cache_write_per_m: input_per_m,
            output_per_m,
        }
    }

    /// USD cost of a single usage record at these prices.
    pub fn cost(&self, u: &Usage) -> f64 {
        const M: f64 = 1_000_000.0;
        (u.input as f64) / M * self.input_per_m
            + (u.cache_read as f64) / M * self.cache_read_per_m
            + (u.cache_write as f64) / M * self.cache_write_per_m
            + (u.output as f64) / M * self.output_per_m
    }
}

/// A model → price registry. Lookup is by longest matching prefix, so a dated or
/// suffixed model id still resolves to its family's price.
#[derive(Debug, Clone, Default)]
pub struct PriceTable {
    prices: HashMap<String, ModelPrice>,
}

impl PriceTable {
    /// An empty table — every lookup returns `None` until you [`insert`].
    pub fn new() -> Self {
        Self::default()
    }

    /// The shipped defaults: the Claude family (per the Anthropic price list) and
    /// DeepSeek's published `deepseek-chat`/`deepseek-reasoner` rates. DeepSeek's
    /// project-specific aliases (e.g. `deepseek-v4-flash`) resolve via the
    /// `deepseek` prefix; override with [`insert`] if your rate differs.
    pub fn builtin() -> Self {
        let mut t = Self::new();
        // Anthropic: cache-read = 0.1x input, 5-minute cache-write = 1.25x input.
        t.insert(
            "claude-opus-4",
            ModelPrice {
                input_per_m: 5.0,
                cache_read_per_m: 0.5,
                cache_write_per_m: 6.25,
                output_per_m: 25.0,
            },
        );
        t.insert(
            "claude-fable-5",
            ModelPrice {
                input_per_m: 10.0,
                cache_read_per_m: 1.0,
                cache_write_per_m: 12.5,
                output_per_m: 50.0,
            },
        );
        t.insert(
            "claude-mythos-5",
            ModelPrice {
                input_per_m: 10.0,
                cache_read_per_m: 1.0,
                cache_write_per_m: 12.5,
                output_per_m: 50.0,
            },
        );
        t.insert(
            "claude-sonnet",
            ModelPrice {
                input_per_m: 3.0,
                cache_read_per_m: 0.3,
                cache_write_per_m: 3.75,
                output_per_m: 15.0,
            },
        );
        t.insert(
            "claude-haiku",
            ModelPrice {
                input_per_m: 1.0,
                cache_read_per_m: 0.1,
                cache_write_per_m: 1.25,
                output_per_m: 5.0,
            },
        );
        // DeepSeek published rates: cache-miss input 0.27, cache-hit read 0.07,
        // output 1.10. No separate cache-write charge, so it bills at input.
        t.insert(
            "deepseek",
            ModelPrice {
                input_per_m: 0.27,
                cache_read_per_m: 0.07,
                cache_write_per_m: 0.27,
                output_per_m: 1.10,
            },
        );
        t
    }

    /// Add or override a model's price. `model_prefix` matches any model id that
    /// starts with it; the longest matching prefix wins at lookup.
    pub fn insert(&mut self, model_prefix: &str, price: ModelPrice) {
        self.prices.insert(model_prefix.to_string(), price);
    }

    /// The price for `model`, by longest matching prefix, or `None` if unknown.
    pub fn price_of(&self, model: &str) -> Option<ModelPrice> {
        self.prices
            .iter()
            .filter(|(prefix, _)| model.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, price)| *price)
    }

    /// USD cost of `usage` for `model`, or `None` if the model has no known price.
    pub fn cost(&self, model: &str, usage: &Usage) -> Option<f64> {
        self.price_of(model).map(|p| p.cost(usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input,
            output,
            ..Default::default()
        }
    }

    #[test]
    fn opus_cost_is_input_plus_output_at_list_price() {
        let t = PriceTable::builtin();
        // 1M input @ $5 + 1M output @ $25 = $30.
        let c = t
            .cost("claude-opus-4-8", &usage(1_000_000, 1_000_000))
            .unwrap();
        assert!((c - 30.0).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn cached_reads_and_writes_bill_at_their_own_tiers() {
        let t = PriceTable::builtin();
        let u = Usage {
            input: 0,
            output: 0,
            cache_read: 1_000_000,  // @ $0.50
            cache_write: 1_000_000, // @ $6.25
            ..Default::default()
        };
        let c = t.cost("claude-opus-4-8", &u).unwrap();
        assert!((c - 6.75).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn longest_prefix_wins_over_family_prefix() {
        let mut t = PriceTable::builtin();
        // A private, cheaper opus alias overrides the family default.
        t.insert("claude-opus-4-8-internal", ModelPrice::flat(1.0, 2.0));
        let c = t
            .cost("claude-opus-4-8-internal-v9", &usage(1_000_000, 0))
            .unwrap();
        assert!((c - 1.0).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn dated_suffix_still_resolves_to_the_family() {
        let t = PriceTable::builtin();
        assert!(t.price_of("claude-sonnet-4-6-20251114").is_some());
        assert!(t.price_of("deepseek-v4-flash").is_some());
    }

    #[test]
    fn unknown_model_has_no_price() {
        let t = PriceTable::builtin();
        assert_eq!(t.cost("some-random-llm", &usage(1000, 1000)), None);
    }

    #[test]
    fn empty_table_knows_nothing() {
        assert_eq!(PriceTable::new().price_of("claude-opus-4-8"), None);
    }
}
