//! Anthropic first-party API list prices, used to estimate what each call would have cost.

use crate::model::short_model;

/// USD per million tokens.
struct Price {
    input: f64,
    output: f64,
    cache_read: f64,
}

impl Price {
    /// Cache reads default to a tenth of the input price.
    const fn new(input: f64, output: f64) -> Self {
        Self { input, output, cache_read: input / 10.0 }
    }

    const fn with_cache_read(self, cache_read: f64) -> Self {
        Self { cache_read, ..self }
    }
}

/// Token counts for one call, with cache writes split by TTL (they're billed differently).
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cache_read: u64,
}

/// Estimated cost in USD, or `None` for models without a known price.
pub fn cost(model: &str, usage: &Usage) -> Option<f64> {
    let p = price(&short_model(model))?;
    let usd = usage.input as f64 * p.input
        + usage.output as f64 * p.output
        + usage.cache_write_5m as f64 * p.input * 1.25
        + usage.cache_write_1h as f64 * p.input * 2.0
        + usage.cache_read as f64 * p.cache_read;
    Some(usd / 1_000_000.0)
}

fn price(model: &str) -> Option<Price> {
    let p = match model {
        "fable-5-1" | "mythos-5-1" => Price::new(10.0, 50.0).with_cache_read(0.25),
        "fable-5" | "mythos-5" => Price::new(10.0, 50.0).with_cache_read(1.0),
        "opus-5-5" => Price::new(4.0, 20.0).with_cache_read(0.20),
        "opus-5" | "opus-4-8" | "opus-4-7" | "opus-4-6" | "opus-4-5" => Price::new(5.0, 25.0),
        "opus-4-1" | "opus-4" => Price::new(15.0, 75.0),
        "sonnet-5" => Price::new(2.0, 10.0),
        "sonnet-4-6" | "sonnet-4-5" | "sonnet-4" | "3-7-sonnet" => Price::new(3.0, 15.0),
        "haiku-4-5" => Price::new(1.0, 5.0),
        "3-5-haiku" => Price::new(0.8, 4.0),
        _ => return None,
    };
    Some(p)
}

/// `$0.42`, `$12.34`, `$1234`
pub fn usd(v: f64) -> String {
    if v < 1000.0 { format!("${v:.2}") } else { format!("${v:.0}") }
}
