use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    /// Provider used when the requested model matches no provider explicitly.
    pub default_provider: Option<String>,
    pub providers: BTreeMap<String, Provider>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// If non-empty, clients must present one of these keys as `Authorization: Bearer <key>`.
    #[serde(default)]
    pub api_keys: Vec<String>,
    #[serde(default = "default_request_timeout")]
    pub request_timeout_secs: u64,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    /// Max request body accepted from a client, in megabytes.
    #[serde(default = "default_body_limit")]
    pub max_body_mb: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// e.g. "https://api.deepseek.com/v1"
    pub base_url: String,
    /// Literal key, or "${ENV_VAR}" to read it from the environment.
    pub api_key: Option<String>,
    /// Alternative to `api_key`: name of an environment variable holding the key.
    pub api_key_env: Option<String>,
    /// Models this provider serves. Used for routing and for `GET /v1/models`.
    #[serde(default)]
    pub models: Vec<String>,
    /// Client-facing name -> upstream name, e.g. "gpt-4o" = "deepseek-chat".
    #[serde(default)]
    pub model_aliases: BTreeMap<String, String>,
    /// Extra headers sent upstream (values support "${ENV_VAR}").
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Ask the upstream API for its model list when `models` is empty.
    #[serde(default = "default_true")]
    pub fetch_models: bool,
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    8600
}
fn default_request_timeout() -> u64 {
    600
}
fn default_connect_timeout() -> u64 {
    15
}
fn default_body_limit() -> usize {
    64
}
fn default_true() -> bool {
    true
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            api_keys: Vec::new(),
            request_timeout_secs: default_request_timeout(),
            connect_timeout_secs: default_connect_timeout(),
            max_body_mb: default_body_limit(),
        }
    }
}

/// Where a request should go: which provider, and under what upstream model name.
pub struct Route<'a> {
    pub name: &'a str,
    pub provider: &'a Provider,
    /// `Some` when the model name must be rewritten before forwarding.
    pub upstream_model: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let mut config: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        config.validate_and_expand()?;
        Ok(config)
    }

    fn validate_and_expand(&mut self) -> Result<()> {
        if self.providers.is_empty() {
            bail!("config defines no [providers.<name>] sections");
        }
        if let Some(default) = &self.default_provider
            && !self.providers.contains_key(default)
        {
            bail!("default_provider = \"{default}\" is not a configured provider");
        }
        for (name, provider) in &mut self.providers {
            provider.base_url = expand_env(&provider.base_url)
                .with_context(|| format!("provider \"{name}\": base_url"))?;
            if !provider.base_url.starts_with("http") {
                bail!("provider \"{name}\": base_url must start with http:// or https://");
            }
            if let Some(key) = &provider.api_key {
                provider.api_key =
                    Some(expand_env(key).with_context(|| format!("provider \"{name}\": api_key"))?);
            }
            if provider.api_key.is_none()
                && let Some(var) = &provider.api_key_env
            {
                let value = std::env::var(var).with_context(|| {
                    format!("provider \"{name}\": api_key_env = \"{var}\" is not set")
                })?;
                provider.api_key = Some(value);
            }
            for (header, value) in &mut provider.headers {
                *value = expand_env(value)
                    .with_context(|| format!("provider \"{name}\": header {header}"))?;
            }
        }
        Ok(())
    }

    /// Pick a provider for a client-supplied model name.
    ///
    /// Resolution order: `provider:model` prefix, alias table, declared model list,
    /// `default_provider`, then the sole provider if only one is configured.
    pub fn resolve(&self, model: Option<&str>) -> Result<Route<'_>> {
        if let Some(model) = model {
            if let Some((prefix, rest)) = model.split_once(':')
                && !rest.is_empty()
                && let Some((name, provider)) = self.providers.get_key_value(prefix)
            {
                let upstream = provider
                    .model_aliases
                    .get(rest)
                    .cloned()
                    .unwrap_or_else(|| rest.to_string());
                return Ok(Route {
                    name,
                    provider,
                    upstream_model: Some(upstream),
                });
            }

            for (name, provider) in self.ordered_providers() {
                if let Some(target) = provider.model_aliases.get(model) {
                    return Ok(Route {
                        name,
                        provider,
                        upstream_model: Some(target.clone()),
                    });
                }
            }
            for (name, provider) in self.ordered_providers() {
                if provider.models.iter().any(|m| m == model) {
                    return Ok(Route {
                        name,
                        provider,
                        upstream_model: None,
                    });
                }
            }
        }

        self.default_route().ok_or_else(|| match model {
            Some(model) => anyhow!(
                "model \"{model}\" matches no provider and no default_provider is configured; use \"<provider>:<model>\" or set default_provider"
            ),
            None => anyhow!("no default_provider is configured"),
        })
    }

    pub fn default_route(&self) -> Option<Route<'_>> {
        let (name, provider) = match &self.default_provider {
            Some(default) => self.providers.get_key_value(default)?,
            None if self.providers.len() == 1 => self.providers.iter().next()?,
            None => return None,
        };
        Some(Route {
            name,
            provider,
            upstream_model: None,
        })
    }

    /// Providers with the default one first, so it wins ambiguous model names.
    fn ordered_providers(&self) -> impl Iterator<Item = (&str, &Provider)> {
        let default = self.default_provider.as_deref();
        let first = default.and_then(|d| self.providers.get_key_value(d));
        first
            .into_iter()
            .chain(
                self.providers
                    .iter()
                    .filter(move |(name, _)| Some(name.as_str()) != default),
            )
            .map(|(name, provider)| (name.as_str(), provider))
    }

    pub fn is_default_provider(&self, name: &str) -> bool {
        match &self.default_provider {
            Some(default) => default == name,
            None => self.providers.len() == 1,
        }
    }

    pub fn client_key_accepted(&self, presented: Option<&str>) -> bool {
        if self.server.api_keys.is_empty() {
            return true;
        }
        presented.is_some_and(|key| self.server.api_keys.iter().any(|k| k == key))
    }
}

impl Provider {
    /// Join `base_url` with the incoming request path, avoiding a duplicated
    /// version segment (e.g. base ".../v1" + path "/v1/chat/completions").
    pub fn upstream_url(&self, path: &str, query: Option<&str>) -> String {
        let base = self.base_url.trim_end_matches('/');
        let last_segment = base.rsplit('/').next().unwrap_or_default();
        let mut rest = path;
        if !last_segment.is_empty() && !last_segment.contains('.') {
            let duplicated = format!("/{last_segment}/");
            if path.starts_with(&duplicated) {
                rest = &path[last_segment.len() + 1..];
            }
        }
        match query {
            Some(query) if !query.is_empty() => format!("{base}{rest}?{query}"),
            _ => format!("{base}{rest}"),
        }
    }

    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }
}

/// Replace a whole-string `${VAR}` (or `env:VAR`) with the environment value.
fn expand_env(value: &str) -> Result<String> {
    let var = value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .or_else(|| value.strip_prefix("env:"));
    match var {
        Some(var) => {
            std::env::var(var).with_context(|| format!("environment variable \"{var}\" is not set"))
        }
        None => Ok(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml_text: &str) -> Config {
        let mut config: Config = toml::from_str(toml_text).expect("parse");
        config.validate_and_expand().expect("validate");
        config
    }

    const TWO_PROVIDERS: &str = r#"
default_provider = "deepseek"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key = "test"
models = ["deepseek-chat"]
model_aliases = { "gpt-4o" = "deepseek-chat" }

[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
api_key = "test2"
models = ["qwen/qwen3-max"]
"#;

    #[test]
    fn routes_explicit_provider_prefix() {
        let config = config(TWO_PROVIDERS);
        let route = config.resolve(Some("openrouter:some/model")).unwrap();
        assert_eq!(route.name, "openrouter");
        assert_eq!(route.upstream_model.as_deref(), Some("some/model"));
    }

    #[test]
    fn routes_alias_and_rewrites_model() {
        let config = config(TWO_PROVIDERS);
        let route = config.resolve(Some("gpt-4o")).unwrap();
        assert_eq!(route.name, "deepseek");
        assert_eq!(route.upstream_model.as_deref(), Some("deepseek-chat"));
    }

    #[test]
    fn routes_declared_model_without_rewrite() {
        let config = config(TWO_PROVIDERS);
        let route = config.resolve(Some("qwen/qwen3-max")).unwrap();
        assert_eq!(route.name, "openrouter");
        assert!(route.upstream_model.is_none());
    }

    #[test]
    fn falls_back_to_default_provider() {
        let config = config(TWO_PROVIDERS);
        assert_eq!(
            config.resolve(Some("unknown-model")).unwrap().name,
            "deepseek"
        );
        assert_eq!(config.resolve(None).unwrap().name, "deepseek");
    }

    #[test]
    fn model_with_colon_that_is_not_a_provider_falls_through() {
        let config = config(TWO_PROVIDERS);
        // e.g. Ollama-style "llama3:8b" must not be read as provider "llama3".
        assert_eq!(config.resolve(Some("llama3:8b")).unwrap().name, "deepseek");
    }

    #[test]
    fn upstream_url_does_not_duplicate_version_segment() {
        let config = config(TWO_PROVIDERS);
        let deepseek = &config.providers["deepseek"];
        assert_eq!(
            deepseek.upstream_url("/v1/chat/completions", None),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            deepseek.upstream_url("/v1/models", Some("limit=5")),
            "https://api.deepseek.com/v1/models?limit=5"
        );
    }

    #[test]
    fn upstream_url_appends_when_base_has_no_version() {
        let mut config: Config = toml::from_str(
            r#"
[providers.local]
base_url = "http://localhost:11434"
"#,
        )
        .unwrap();
        config.validate_and_expand().unwrap();
        assert_eq!(
            config.providers["local"].upstream_url("/v1/chat/completions", None),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn client_auth_is_open_until_keys_are_configured() {
        let mut config = config(TWO_PROVIDERS);
        assert!(config.client_key_accepted(None));
        config.server.api_keys = vec!["secret".into()];
        assert!(!config.client_key_accepted(None));
        assert!(!config.client_key_accepted(Some("nope")));
        assert!(config.client_key_accepted(Some("secret")));
    }
}
