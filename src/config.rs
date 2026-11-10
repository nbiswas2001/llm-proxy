use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub providers: BTreeMap<String, Provider>,
    /// The provider serving this run: `--provider` if it named one, otherwise the
    /// one marked `default = true`. Resolved at load time.
    #[serde(skip)]
    active_name: String,
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
    /// Folder that receives one transcript file per day of every request and
    /// its response. Relative paths sit beside the config file; "" turns it off.
    #[serde(default = "default_log_dir")]
    pub log_dir: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// The provider used when the command line does not name one. At most one
    /// provider in the file may set it.
    #[serde(default, rename = "default")]
    pub is_default: bool,
    /// e.g. "https://api.deepseek.com/v1"
    pub base_url: String,
    /// Literal key, or "${ENV_VAR}" to read it from the environment or .env file.
    pub api_key: Option<String>,
    /// Alternative to `api_key`: name of an environment variable holding the key.
    pub api_key_env: Option<String>,
    /// Wire protocol this provider speaks, and therefore the one clients of the
    /// proxy must speak while it is active. Bodies are forwarded untouched
    /// either way; what differs is how the key is presented upstream. Required:
    /// getting it wrong is a 401 from upstream, so every provider states it.
    pub api: Api,
    /// Extra headers sent upstream (values support "${ENV_VAR}").
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Where `--balance` should look for this provider's remaining credit. Only
    /// needed for a provider `balance.rs` has no built-in endpoint for.
    pub balance: Option<BalanceEndpoint>,
}

/// The two request protocols in circulation. There is no translation between
/// them: `api = "anthropic"` means the proxy exposes Anthropic's Messages API,
/// so clients must speak it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Api {
    /// `Authorization: Bearer <key>`, the shape everything else here uses.
    Openai,
    /// `x-api-key: <key>` plus the `anthropic-version` header their API requires.
    Anthropic,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BalanceEndpoint {
    /// Full URL, e.g. "https://api.example.com/account/credits". The provider's
    /// key and headers are attached; the JSON that comes back is printed as-is,
    /// since nothing is known about its shape. Supports "${ENV_VAR}".
    pub url: String,
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
fn default_log_dir() -> String {
    "logs".to_string()
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
            log_dir: default_log_dir(),
        }
    }
}

impl Config {
    /// `chosen` is the `--provider` name, when one was given; otherwise the
    /// provider marked `default = true` serves the run.
    pub fn load(path: &Path, chosen: Option<&str>) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let mut config =
            parse(&text).with_context(|| format!("parsing config file {}", path.display()))?;
        config.validate_and_expand(chosen)?;
        Ok(config)
    }

    fn validate_and_expand(&mut self, chosen: Option<&str>) -> Result<()> {
        if self.providers.is_empty() {
            bail!("config defines no [providers.<name>] sections");
        }

        let defaults: Vec<&str> = self
            .providers
            .iter()
            .filter(|(_, provider)| provider.is_default)
            .map(|(name, _)| name.as_str())
            .collect();
        let all = |config: &Self| {
            config
                .providers
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        };
        // Two defaults is an ambiguous file however the run was started.
        if defaults.len() > 1 {
            bail!(
                "providers {} are all marked default; at most one may set default = true",
                defaults.join(", ")
            );
        }
        self.active_name = match chosen {
            // A named provider wins; a name that is not in the file is a typo
            // worth stopping for, not a reason to fall back to the default.
            Some(name) if self.providers.contains_key(name) => name.to_string(),
            Some(name) => bail!(
                "--provider \"{name}\" is not in the config; available: {}",
                all(self)
            ),
            None => match defaults.first() {
                Some(name) => name.to_string(),
                None => bail!(
                    "no default provider; set default = true on one of: {}, \
                     or name one with --provider",
                    all(self)
                ),
            },
        };

        // Only the active provider is validated: an inactive one may reference a key
        // that is not present, which is what makes commenting-in a provider cheap.
        let name = self.active_name.clone();
        let provider = self
            .providers
            .get_mut(&name)
            .expect("active provider was just found");

        provider.base_url =
            expand_env(&provider.base_url).with_context(|| format!("provider \"{name}\""))?;
        if !provider.base_url.starts_with("http") {
            bail!("provider \"{name}\": base_url must start with http:// or https://");
        }
        if let Some(raw) = provider.api_key.clone() {
            let key = expand_env(&raw).with_context(|| format!("provider \"{name}\": api_key"))?;
            if key.trim().is_empty() {
                bail!(
                    "provider \"{name}\": api_key = \"{raw}\" resolved to an empty value; \
                     fill it in (e.g. in the .env file) or drop the api_key line if this \
                     provider needs no key"
                );
            }
            provider.api_key = Some(key);
        }
        if provider.api_key.is_none()
            && let Some(var) = &provider.api_key_env
        {
            let value = std::env::var(var).with_context(|| {
                format!("provider \"{name}\": api_key_env = \"{var}\" is not set")
            })?;
            if value.trim().is_empty() {
                bail!(
                    "provider \"{name}\": api_key_env = \"{var}\" is set but empty; \
                     fill it in (e.g. in the .env file)"
                );
            }
            provider.api_key = Some(value);
        }
        for (header, value) in &mut provider.headers {
            *value = expand_env(value)
                .with_context(|| format!("provider \"{name}\": header {header}"))?;
        }
        if let Some(balance) = &mut provider.balance {
            balance.url = expand_env(&balance.url)
                .with_context(|| format!("provider \"{name}\": balance.url"))?;
            if !balance.url.starts_with("http") {
                bail!("provider \"{name}\": balance.url must start with http:// or https://");
            }
        }
        Ok(())
    }

    /// The single provider every request of this run is forwarded to.
    pub fn active(&self) -> (&str, &Provider) {
        let provider = self
            .providers
            .get(&self.active_name)
            .expect("validated at load time");
        (self.active_name.as_str(), provider)
    }

    /// Catch a model name left over from another provider, e.g. "openrouter:gpt-4o"
    /// once openrouter has been switched off, instead of passing it upstream to be
    /// rejected as an unknown model.
    pub fn reject_stale_prefix(&self, model: &str) -> Result<(), String> {
        if let Some((prefix, _)) = model.split_once(':')
            && prefix != self.active_name
            && self.providers.contains_key(prefix)
        {
            return Err(format!(
                "model \"{model}\" names provider \"{prefix}\", but \"{}\" is the active one; \
                 drop the \"{prefix}:\" prefix, or start with --provider {prefix}",
                self.active_name
            ));
        }
        Ok(())
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

    pub fn is_anthropic(&self) -> bool {
        self.api == Api::Anthropic
    }

    /// Whether the config already pins Anthropic's version header, in which case
    /// the proxy must not add its own default on top.
    pub fn sets_anthropic_version(&self) -> bool {
        self.headers
            .keys()
            .any(|name| name.eq_ignore_ascii_case("anthropic-version"))
    }
}

/// Deserialize the file, spelling out the one required field a hand-written
/// config is most likely to be missing.
fn parse(text: &str) -> Result<Config> {
    toml::from_str(text).map_err(|err| {
        let message = err.to_string();
        let hint = if message.contains("missing field `api`") {
            "\nhint: every [providers.<name>] sets api = \"openai\", or \"anthropic\" for Claude"
        } else if message.contains("unknown field `active`") {
            "\nhint: `active` is now `default` - the provider used when --provider names none"
        } else {
            ""
        };
        anyhow::anyhow!("{err}{hint}")
    })
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

    fn load(toml_text: &str) -> Result<Config> {
        select(toml_text, None)
    }

    fn select(toml_text: &str, chosen: Option<&str>) -> Result<Config> {
        let mut config = parse(toml_text).expect("parse");
        config.validate_and_expand(chosen)?;
        Ok(config)
    }

    const TWO_PROVIDERS: &str = r#"
[providers.deepseek]
default = true
api = "openai"
base_url = "https://api.deepseek.com/v1"
api_key = "test"

[providers.llamacpp]
api = "openai"
base_url = "http://127.0.0.1:8503/v1"
"#;

    #[test]
    fn every_provider_states_the_protocol_it_speaks() {
        let config = load(TWO_PROVIDERS).expect("valid");
        let (_, deepseek) = config.active();
        assert_eq!(deepseek.api, Api::Openai);
        assert!(!deepseek.is_anthropic());

        let claude = load(
            r#"
[providers.claude]
default = true
api = "anthropic"
base_url = "https://api.anthropic.com/v1"
api_key = "sk-ant-test"
"#,
        )
        .expect("valid");
        assert!(claude.active().1.is_anthropic());
    }

    #[test]
    fn a_provider_with_no_api_field_is_rejected() {
        // Including an inactive one: the whole file is deserialized, and guessing
        // the protocol would show up as a 401 from upstream rather than here.
        let error = parse(
            r#"
[providers.deepseek]
default = true
base_url = "https://api.deepseek.com/v1"
"#,
        )
        .expect_err("missing api")
        .to_string();
        assert!(error.contains("missing field `api`"), "{error}");
        assert!(error.contains("anthropic"), "{error}");
    }

    #[test]
    fn an_unknown_api_flavor_is_a_startup_error() {
        let error = parse(
            r#"
[providers.claude]
default = true
api = "gemini"
base_url = "https://example.com/v1"
"#,
        )
        .expect_err("unknown flavor");
        assert!(error.to_string().contains("anthropic"), "{error}");
    }

    #[test]
    fn the_active_provider_receives_every_request() {
        let config = load(TWO_PROVIDERS).unwrap();
        let (name, provider) = config.active();
        assert_eq!(name, "deepseek");
        assert_eq!(provider.api_key(), Some("test"));
    }

    #[test]
    fn two_default_providers_are_rejected() {
        let toml_text = TWO_PROVIDERS.replace(
            "[providers.llamacpp]\napi",
            "[providers.llamacpp]\ndefault = true\napi",
        );
        let err = load(&toml_text).unwrap_err().to_string();
        assert!(err.contains("deepseek, llamacpp"), "{err}");
        assert!(err.contains("at most one"), "{err}");
    }

    #[test]
    fn a_named_provider_wins_over_the_default() {
        let config = select(TWO_PROVIDERS, Some("llamacpp")).unwrap();
        assert_eq!(config.active().0, "llamacpp");
    }

    #[test]
    fn a_provider_name_that_is_not_in_the_config_is_an_error() {
        let err = select(TWO_PROVIDERS, Some("llamacp"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not in the config"), "{err}");
        assert!(err.contains("deepseek, llamacpp"), "{err}");
    }

    #[test]
    fn a_default_is_needed_only_when_no_provider_is_named() {
        let toml_text = TWO_PROVIDERS.replace("default = true\n", "");
        let err = load(&toml_text).unwrap_err().to_string();
        assert!(err.contains("no default provider"), "{err}");
        assert!(err.contains("deepseek, llamacpp"), "{err}");
        // ... but the run is fine when the command line names one.
        let config = select(&toml_text, Some("llamacpp")).unwrap();
        assert_eq!(config.active().0, "llamacpp");
    }

    #[test]
    fn an_inactive_provider_may_reference_a_missing_key() {
        // Only the active provider is expanded, so this must not fail to load.
        let config = load(
            r#"
[providers.deepseek]
default = true
api = "openai"
base_url = "https://api.deepseek.com/v1"
api_key = "test"

[providers.openrouter]
api = "openai"
base_url = "https://openrouter.ai/api/v1"
api_key = "${DEFINITELY_NOT_SET_12345}"
"#,
        )
        .unwrap();
        assert_eq!(config.active().0, "deepseek");
    }

    #[test]
    fn an_empty_key_is_rejected() {
        unsafe { std::env::set_var("LLM_PROXY_TEST_BLANK", "  ") };
        let err = load(
            r#"
[providers.deepseek]
default = true
api = "openai"
base_url = "https://api.deepseek.com/v1"
api_key = "${LLM_PROXY_TEST_BLANK}"
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("empty value"), "{err}");
    }

    #[test]
    fn a_prefix_naming_an_inactive_provider_is_an_error() {
        let config = load(TWO_PROVIDERS).unwrap();
        let err = config.reject_stale_prefix("llamacpp:qwen").unwrap_err();
        assert!(err.contains("\"deepseek\" is the active one"), "{err}");
        assert!(err.contains("--provider llamacpp"), "{err}");
    }

    #[test]
    fn ordinary_model_names_pass_through_untouched() {
        let config = load(TWO_PROVIDERS).unwrap();
        // Including names that merely contain a colon, e.g. Ollama-style tags.
        assert!(config.reject_stale_prefix("deepseek-v4-flash").is_ok());
        assert!(config.reject_stale_prefix("llama3:8b").is_ok());
        assert!(config.reject_stale_prefix("deepseek:anything").is_ok());
    }

    #[test]
    fn upstream_url_does_not_duplicate_version_segment() {
        let config = load(TWO_PROVIDERS).unwrap();
        let (_, provider) = config.active();
        assert_eq!(
            provider.upstream_url("/v1/chat/completions", None),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            provider.upstream_url("/v1/models", Some("limit=5")),
            "https://api.deepseek.com/v1/models?limit=5"
        );
    }

    #[test]
    fn upstream_url_appends_when_base_has_no_version() {
        let config = load(
            r#"
[providers.local]
default = true
api = "openai"
base_url = "http://localhost:11434"
"#,
        )
        .unwrap();
        assert_eq!(
            config.active().1.upstream_url("/v1/chat/completions", None),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn client_auth_is_open_until_keys_are_configured() {
        let mut config = load(TWO_PROVIDERS).unwrap();
        assert!(config.client_key_accepted(None));
        config.server.api_keys = vec!["secret".into()];
        assert!(!config.client_key_accepted(None));
        assert!(!config.client_key_accepted(Some("nope")));
        assert!(config.client_key_accepted(Some("secret")));
    }
}
