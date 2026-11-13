//! Credential lookup and the mode-0600 local credential store.

use std::path::Path;

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::config::{Config, ProviderConfig};
use crate::paths::{AaykraPaths, atomic_write, secure_file_permissions};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CredentialEntry {
    pub api_key: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub providers: IndexMap<String, CredentialEntry>,
}

impl Credentials {
    pub fn load(paths: &AaykraPaths) -> Result<Self> {
        let path = paths.credentials();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let creds: Self = toml::from_str(&text)
            .with_context(|| format!("invalid credential store {}", path.display()))?;
        Ok(creds)
    }

    pub fn set(paths: &AaykraPaths, provider: &str, api_key: &str) -> Result<()> {
        let mut creds = Self::load(paths)?;
        creds.providers.insert(
            provider.to_string(),
            CredentialEntry {
                api_key: api_key.trim().to_string(),
            },
        );
        creds.save(paths)
    }

    pub fn remove(paths: &AaykraPaths, provider: &str) -> Result<()> {
        let mut creds = Self::load(paths)?;
        creds.providers.shift_remove(provider);
        creds.save(paths)
    }

    fn save(&self, paths: &AaykraPaths) -> Result<()> {
        paths.ensure()?;
        let path = paths.credentials();
        let body = toml::to_string_pretty(self).context("could not serialize credentials")?;
        let content = format!(
            "# Aaykra Code credentials. Keep private. Prefer environment variables in CI.\n{body}"
        );
        atomic_write(&path, content.as_bytes())?;
        secure_file_permissions(&path)?;
        Ok(())
    }

    #[must_use]
    pub fn get(&self, provider: &str) -> Option<&str> {
        self.providers
            .get(provider)
            .map(|entry| entry.api_key.as_str())
            .filter(|key| !key.trim().is_empty())
    }
}

/// Resolve a key without ever logging the secret.
///
/// Order: literal/expanded config key, configured env name, credential store,
/// standard provider env names, generic `AAYKRA_API_KEY` (not for local
/// providers). Local providers may legitimately return `None`.
pub fn resolve_api_key(
    _config: &Config,
    provider_name: &str,
    provider: &ProviderConfig,
    paths: &AaykraPaths,
) -> Result<Option<String>> {
    if let Some(key) = provider.api_key.as_deref().and_then(expand_secret) {
        return Ok(Some(key));
    }
    if let Some(env_name) = &provider.api_key_env
        && let Ok(key) = std::env::var(env_name)
        && !key.trim().is_empty()
    {
        return Ok(Some(key));
    }
    let credentials = Credentials::load(paths)?;
    if let Some(key) = credentials.get(provider_name) {
        return Ok(Some(key.to_string()));
    }
    for env_name in provider_env_names(provider_name) {
        if let Ok(key) = std::env::var(env_name)
            && !key.trim().is_empty()
        {
            return Ok(Some(key));
        }
    }
    if !provider.requires_key {
        return Ok(None);
    }
    if let Ok(key) = std::env::var("AAYKRA_API_KEY")
        && !key.trim().is_empty()
    {
        return Ok(Some(key));
    }
    Ok(None)
}

fn expand_secret(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(name) = trimmed.strip_prefix("env:") {
        return std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    }
    if trimmed.starts_with("${") && trimmed.ends_with('}') {
        let name = &trimmed[2..trimmed.len() - 1];
        return std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    }
    Some(trimmed.to_string())
}

fn provider_env_names(provider: &str) -> &'static [&'static str] {
    match provider {
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "gemini" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "together" => &["TOGETHER_API_KEY"],
        "fireworks" => &["FIREWORKS_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        _ => &[],
    }
}

#[must_use]
pub fn available_provider_from_env(config: &Config) -> Option<String> {
    for (name, provider) in config.enabled_providers() {
        if provider
            .api_key_env
            .as_ref()
            .is_some_and(|var| std::env::var(var).is_ok())
            || provider_env_names(name)
                .iter()
                .any(|var| std::env::var(var).is_ok())
            || !provider.requires_key
        {
            return Some(name.to_string());
        }
    }
    None
}

#[must_use]
pub fn require_api_key(
    config: &Config,
    provider_name: &str,
    provider: &ProviderConfig,
    paths: &AaykraPaths,
) -> Result<Option<String>> {
    let key = resolve_api_key(config, provider_name, provider, paths)?;
    if provider.requires_key && key.is_none() {
        bail!(
            "no API key for provider `{provider_name}`. Set {}, run `akc auth set {provider_name}`, or configure providers.{provider_name}.api_key",
            provider
                .api_key_env
                .as_deref()
                .unwrap_or("the provider's API key environment variable")
        );
    }
    Ok(key)
}

#[must_use]
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "••••".into();
    }
    let prefix: String = chars.iter().take(4).collect();
    let suffix: String = chars.iter().skip(chars.len() - 4).collect();
    format!("{prefix}…{suffix}")
}

/// Write an initial credential store with restricted permissions, if absent.
pub fn ensure_credentials_file(paths: &AaykraPaths) -> Result<()> {
    let path = paths.credentials();
    if !path.exists() {
        atomic_write(&path, b"# Aaykra Code credentials.\n[providers]\n")?;
        secure_file_permissions(&path)?;
    }
    Ok(())
}

#[must_use]
pub fn credentials_exist(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_secret() {
        assert_eq!(mask_key("sk-1234567890"), "sk-1…7890");
        assert_eq!(mask_key("short"), "••••");
    }

    #[test]
    fn empty_secret_is_none() {
        assert_eq!(expand_secret("   "), None);
    }
}
