//! Model references, context windows, and cost estimation.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Config, ProviderConfig};
use crate::types::Usage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

impl ModelRef {
    #[must_use]
    pub fn display(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub reference: ModelRef,
    pub provider: ProviderConfig,
}

/// Resolve a provider/model pair. `model` may be `provider/model`; an explicit
/// `provider` argument disables prefix splitting so OpenRouter-style model IDs
/// work naturally.
pub fn resolve_model(
    config: &Config,
    model: Option<&str>,
    provider: Option<&str>,
) -> Result<ResolvedModel> {
    let requested_model = model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| config.model.clone());

    let provider_name = if let Some(explicit) = provider {
        if !config.providers.contains_key(explicit) {
            bail!(
                "unknown provider `{explicit}`. Known providers: {}",
                config
                    .providers
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        explicit.to_string()
    } else if let Some(raw) = &requested_model {
        if let Some((prefix, rest)) = raw.split_once('/')
            && config.providers.contains_key(prefix)
            && !rest.is_empty()
        {
            prefix.to_string()
        } else {
            infer_provider(config, raw)?
        }
    } else if let Some(default) = &config.default_provider {
        if !config.providers.contains_key(default) {
            bail!("default_provider `{default}` is not configured");
        }
        default.clone()
    } else {
        infer_provider(config, "")?
    };

    let provider_config = config
        .providers
        .get(&provider_name)
        .with_context(|| format!("provider `{provider_name}` is not configured"))?;
    if !provider_config.enabled {
        bail!("provider `{provider_name}` is disabled");
    }

    let mut model_name = requested_model.unwrap_or_default();
    if provider.is_none()
        && let Some((prefix, rest)) = model_name.split_once('/')
        && prefix == provider_name
        && config.providers.contains_key(prefix)
        && !rest.is_empty()
    {
        model_name = rest.to_string();
    }
    if model_name.trim().is_empty() {
        model_name = provider_config
            .default_model
            .clone()
            .or_else(|| provider_config.known_models.first().cloned())
            .with_context(|| {
                format!(
                    "no model configured for provider `{provider_name}`; set model or provide --model"
                )
            })?;
    }

    Ok(ResolvedModel {
        reference: ModelRef {
            provider: provider_name,
            model: model_name,
        },
        provider: provider_config.clone(),
    })
}

fn infer_provider(config: &Config, model: &str) -> Result<String> {
    let lower = model.to_ascii_lowercase();
    let model_hint = if lower.starts_with("claude") {
        Some("anthropic")
    } else if lower.starts_with("gpt")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        Some("openai")
    } else if lower.starts_with("gemini") {
        Some("gemini")
    } else if lower.starts_with("deepseek") {
        Some("deepseek")
    } else {
        None
    };
    if let Some(provider) = model_hint
        && config.providers.get(provider).is_some_and(|p| p.enabled)
    {
        return Ok(provider.to_string());
    }

    if let Some(default) = &config.default_provider {
        return Ok(default.clone());
    }

    // Prefer providers with a discoverable key.
    for candidate in [
        "anthropic",
        "openai",
        "deepseek",
        "groq",
        "gemini",
        "openrouter",
        "mistral",
    ] {
        if let Some(provider) = config.providers.get(candidate)
            && provider.enabled
            && provider_key_present(provider)
        {
            return Ok(candidate.to_string());
        }
    }

    // Local providers do not need keys.
    for (name, provider) in &config.providers {
        if provider.enabled && !provider.requires_key {
            return Ok(name.clone());
        }
    }

    // A custom single provider is a reasonable fallback.
    let enabled: Vec<_> = config
        .providers
        .iter()
        .filter(|(_, p)| p.enabled)
        .map(|(n, _)| n.clone())
        .collect();
    if enabled.len() == 1 {
        return Ok(enabled[0].clone());
    }

    bail!(
        "could not choose a provider. Set default_provider, pass --provider, or configure an API key"
    )
}

fn provider_key_present(provider: &ProviderConfig) -> bool {
    provider.api_key.as_ref().is_some_and(|v| !v.is_empty())
        || provider
            .api_key_env
            .as_ref()
            .is_some_and(|name| std::env::var(name).is_ok())
}

#[must_use]
pub fn context_window(provider: &str, model: &str) -> usize {
    let m = model.to_ascii_lowercase();
    if provider == "gemini" || m.starts_with("gemini") {
        1_048_576
    } else if m.contains("claude") {
        200_000
    } else if m.starts_with("gpt-5") {
        400_000
    } else if m.starts_with("gpt-4.1") {
        1_047_576
    } else if m.starts_with("gpt-4o") {
        128_000
    } else if m.starts_with("deepseek") {
        128_000
    } else if matches!(provider, "ollama" | "lmstudio" | "vllm" | "llamacpp") {
        // Conservative; models frequently advertise 32k-128k.
        32_768
    } else if provider == "mistral" {
        128_000
    } else {
        128_000
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Pricing {
    /// USD per million input tokens.
    pub input: f64,
    /// USD per million output tokens.
    pub output: f64,
    /// USD per million cache-read tokens.
    #[serde(default)]
    pub cache_read: f64,
    /// USD per million cache-write tokens.
    #[serde(default)]
    pub cache_write: f64,
}

impl Pricing {
    const fn new(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Self {
        Self {
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    #[must_use]
    pub fn zero() -> Self {
        Self::new(0.0, 0.0, 0.0, 0.0)
    }
}

/// Approximate public list prices. Override with `[pricing."provider/model"]`
/// in config; actual provider bills are authoritative.
#[must_use]
pub fn pricing_for(config: &Config, provider: &str, model: &str) -> Pricing {
    for key in [
        format!("{provider}/{model}"),
        model.to_string(),
        provider.to_string(),
    ] {
        if let Some(pricing) = config.pricing.get(&key) {
            return *pricing;
        }
    }
    if config
        .pricing
        .iter()
        .any(|(key, _)| key == "*" || key == "default")
    {
        return config
            .pricing
            .get("*")
            .or_else(|| config.pricing.get("default"))
            .copied()
            .unwrap_or_else(Pricing::zero);
    }

    let m = model.to_ascii_lowercase();
    if matches!(provider, "ollama" | "lmstudio" | "vllm" | "llamacpp") {
        return Pricing::zero();
    }
    if m.contains("opus") {
        return Pricing::new(15.0, 75.0, 1.5, 18.75);
    }
    if m.contains("sonnet") {
        return Pricing::new(3.0, 15.0, 0.3, 3.75);
    }
    if m.contains("haiku") {
        return Pricing::new(1.0, 5.0, 0.1, 1.25);
    }
    if m.contains("gemini-2.5-pro") {
        return Pricing::new(1.25, 10.0, 0.31, 0.0);
    }
    if m.contains("gemini") && m.contains("flash") {
        return Pricing::new(0.30, 2.50, 0.075, 0.0);
    }
    if m.contains("deepseek-reasoner") {
        return Pricing::new(0.55, 2.19, 0.14, 0.0);
    }
    if m.contains("deepseek") {
        return Pricing::new(0.27, 1.10, 0.07, 0.0);
    }
    if m.starts_with("gpt-5") {
        return Pricing::new(1.25, 10.0, 0.125, 0.0);
    }
    if m.starts_with("gpt-4.1") {
        return Pricing::new(2.0, 8.0, 0.50, 0.0);
    }
    if m.starts_with("gpt-4o") {
        return Pricing::new(2.50, 10.0, 1.25, 0.0);
    }
    if m.starts_with("o1") || m.starts_with("o3") {
        return Pricing::new(10.0, 40.0, 2.5, 0.0);
    }
    if m.contains("mistral-large") {
        return Pricing::new(2.0, 6.0, 0.0, 0.0);
    }
    if provider == "groq" {
        return Pricing::new(0.59, 0.79, 0.0, 0.0);
    }
    // A deliberately non-zero neutral fallback makes missing prices visible.
    Pricing::new(3.0, 15.0, 0.3, 3.75)
}

#[must_use]
pub fn estimate_cost(config: &Config, provider: &str, model: &str, usage: &Usage) -> f64 {
    let price = pricing_for(config, provider, model);
    let regular_input = usage
        .input_tokens
        .saturating_sub(usage.cache_read_input_tokens)
        .saturating_sub(usage.cache_creation_input_tokens);
    let cost = regular_input as f64 * price.input / 1_000_000.0
        + usage.output_tokens as f64 * price.output / 1_000_000.0
        + usage.cache_read_input_tokens as f64 * price.cache_read / 1_000_000.0
        + usage.cache_creation_input_tokens as f64 * price.cache_write / 1_000_000.0;
    (cost * 1_000_000.0).round() / 1_000_000.0
}

#[must_use]
pub fn format_cost(cost: f64) -> String {
    if cost <= 0.0 {
        "$0.000".into()
    } else if cost < 0.01 {
        format!("${cost:.4}")
    } else {
        format!("${cost:.3}")
    }
}

/// Rough token count for context management. Providers report exact usage after
/// each call, but compaction must decide before the call.
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    (text.chars().count() / 4).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_provider_keeps_openrouter_model() {
        let config = Config::default();
        let resolved = resolve_model(
            &config,
            Some("anthropic/claude-sonnet-4.5"),
            Some("openrouter"),
        )
        .unwrap();
        assert_eq!(resolved.reference.provider, "openrouter");
        assert_eq!(resolved.reference.model, "anthropic/claude-sonnet-4.5");
    }

    #[test]
    fn provider_prefix_splits() {
        let config = Config::default();
        let resolved = resolve_model(&config, Some("openai/gpt-5.1"), None).unwrap();
        assert_eq!(resolved.reference.provider, "openai");
        assert_eq!(resolved.reference.model, "gpt-5.1");
    }

    #[test]
    fn model_hint_selects_provider() {
        let config = Config::default();
        let resolved = resolve_model(&config, Some("claude-haiku-4-5"), None).unwrap();
        assert_eq!(resolved.reference.provider, "anthropic");
    }

    #[test]
    fn cost_uses_cache_prices() {
        let config = Config::default();
        let usage = Usage {
            input_tokens: 1_000_000,
            cache_read_input_tokens: 500_000,
            output_tokens: 100_000,
            ..Usage::default()
        };
        let cost = estimate_cost(&config, "anthropic", "claude-sonnet-4-5", &usage);
        assert!(cost > 0.0);
    }
}
