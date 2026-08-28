//! `--balance`: ask the active provider how much credit is left.
//!
//! Balance is not part of the OpenAI protocol, so unlike the forwarding path this
//! is per-provider knowledge: a small table keyed on the host of `base_url`, plus
//! a `[providers.<name>.balance]` escape hatch in the config for anything the
//! table does not know. Providers that publish no such endpoint at all (OpenAI,
//! Anthropic) and local servers report that plainly instead of guessing.

use anyhow::{Context, Result, bail};
use reqwest::{Client, Url};
use serde::Deserialize;
use tracing::debug;

use crate::config::{Config, Provider};
use crate::proxy::apply_provider_auth;

/// A provider whose balance endpoint and response shape are known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Known {
    DeepSeek,
    OpenRouter,
}

/// What gets printed for one provider: the headline number, then whatever else
/// the provider volunteered.
struct Report {
    remaining: String,
    details: Vec<(String, String)>,
}

/// Query the active provider and print its balance. Any failure - no known
/// endpoint, a refused key, an unreadable response - is an error, so `--balance`
/// is usable from a script.
pub async fn report(config: &Config, http: &Client) -> Result<()> {
    let (name, provider) = config.active();

    // An explicit endpoint wins over the built-in table, so a provider it does
    // not know - or one that moves its endpoint - needs no code change.
    if let Some(balance) = &provider.balance {
        let body = fetch(http, provider, &balance.url).await?;
        println!("provider \"{name}\" -> {}", balance.url);
        // Nothing is known about this shape, so show it rather than interpret it.
        match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(value) => println!("{}", serde_json::to_string_pretty(&value)?),
            Err(_) => println!("{}", body.trim()),
        }
        return Ok(());
    }

    let Some(known) = known_for(&provider.base_url) else {
        bail!(
            "provider \"{name}\" ({}) has no known balance endpoint - a balance API is not \
             part of the OpenAI protocol, so it exists only where the provider offers one. \
             If it does, name it in the config:\n\n  [providers.{name}.balance]\n  \
             url = \"https://.../account/credits\"",
            provider.base_url
        );
    };

    let origin = origin(&provider.base_url)?;
    let (url, report) = match known {
        Known::DeepSeek => {
            // Documented at the origin root, not under the /v1 base_url.
            let url = format!("{origin}/user/balance");
            let body = fetch(http, provider, &url).await?;
            let balance: DeepSeekBalance = parse(&url, &body)?;
            (url, deepseek_report(&balance))
        }
        Known::OpenRouter => {
            let url = format!("{origin}/api/v1/credits");
            let body = fetch(http, provider, &url).await?;
            let credits: OpenRouterEnvelope<OpenRouterCredits> = parse(&url, &body)?;
            // A key may carry its own spend cap below the account balance. That is
            // supplementary, so failing to read it must not sink the report.
            let key_url = format!("{origin}/api/v1/key");
            let key = match fetch(http, provider, &key_url).await {
                Ok(body) => serde_json::from_str::<OpenRouterEnvelope<OpenRouterKey>>(&body)
                    .map(|envelope| envelope.data)
                    .ok(),
                Err(err) => {
                    debug!("key limit unavailable: {err}");
                    None
                }
            };
            (url, openrouter_report(&credits.data, key.as_ref()))
        }
    };

    println!("provider \"{name}\" -> {url}");
    println!("  {:<20} {}", "remaining", report.remaining);
    for (label, value) in &report.details {
        println!("  {label:<20} {value}");
    }
    Ok(())
}

/// Providers with a built-in endpoint, recognised by the host of `base_url` so a
/// regional or proxied host with the same API still matches on its own hostname.
fn known_for(base_url: &str) -> Option<Known> {
    let host = Url::parse(base_url).ok()?.host_str()?.to_ascii_lowercase();
    match host.trim_start_matches("www.") {
        "api.deepseek.com" => Some(Known::DeepSeek),
        "openrouter.ai" => Some(Known::OpenRouter),
        _ => None,
    }
}

/// Scheme and host of `base_url`, without the API path - balance endpoints tend
/// to sit beside `/v1` rather than inside it.
fn origin(base_url: &str) -> Result<String> {
    let url = Url::parse(base_url).with_context(|| format!("parsing base_url {base_url}"))?;
    Ok(url.origin().ascii_serialization())
}

async fn fetch(http: &Client, provider: &Provider, url: &str) -> Result<String> {
    let request = apply_provider_auth(http.get(url), provider);
    let response = request
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("reading the response from {url}"))?;
    if !status.is_success() {
        bail!("{url} returned {status}: {}", short(&body));
    }
    Ok(body)
}

fn parse<T: serde::de::DeserializeOwned>(url: &str, body: &str) -> Result<T> {
    serde_json::from_str(body)
        .with_context(|| format!("unexpected response from {url}: {}", short(body)))
}

/// Keep an unexpected body from flooding the terminal.
fn short(body: &str) -> String {
    let body = body.trim();
    match body.char_indices().nth(300) {
        Some((index, _)) => format!("{}...", &body[..index]),
        None => body.to_string(),
    }
}

fn money(amount: f64) -> String {
    format!("{amount:.2}")
}

// The response types below are deliberately lenient - no `deny_unknown_fields`,
// and optional where the provider documents a field as sometimes absent - so a
// new field upstream does not turn into a parse error here.

/// `GET https://api.deepseek.com/user/balance`
#[derive(Debug, Deserialize)]
struct DeepSeekBalance {
    #[serde(default)]
    is_available: bool,
    #[serde(default)]
    balance_infos: Vec<DeepSeekBalanceInfo>,
}

#[derive(Debug, Deserialize)]
struct DeepSeekBalanceInfo {
    currency: String,
    total_balance: String,
    granted_balance: Option<String>,
    topped_up_balance: Option<String>,
}

fn deepseek_report(balance: &DeepSeekBalance) -> Report {
    let amount = |value: &str, currency: &str| format!("{value} {currency}");
    // One entry per currency; an account can hold more than one.
    let remaining = if balance.balance_infos.is_empty() {
        "unknown".to_string()
    } else {
        balance
            .balance_infos
            .iter()
            .map(|info| amount(&info.total_balance, &info.currency))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut details = Vec::new();
    for info in &balance.balance_infos {
        if let Some(granted) = &info.granted_balance {
            details.push(("granted".to_string(), amount(granted, &info.currency)));
        }
        if let Some(topped_up) = &info.topped_up_balance {
            details.push(("topped up".to_string(), amount(topped_up, &info.currency)));
        }
    }
    details.push((
        "accepting requests".to_string(),
        if balance.is_available { "yes" } else { "no" }.to_string(),
    ));
    Report { remaining, details }
}

#[derive(Debug, Deserialize)]
struct OpenRouterEnvelope<T> {
    data: T,
}

/// `GET https://openrouter.ai/api/v1/credits`
#[derive(Debug, Deserialize)]
struct OpenRouterCredits {
    total_credits: f64,
    total_usage: f64,
}

/// `GET https://openrouter.ai/api/v1/key`
#[derive(Debug, Deserialize)]
struct OpenRouterKey {
    limit: Option<f64>,
    limit_remaining: Option<f64>,
    is_free_tier: Option<bool>,
}

fn openrouter_report(credits: &OpenRouterCredits, key: Option<&OpenRouterKey>) -> Report {
    let mut details = vec![
        (
            "granted".to_string(),
            format!("{} credits", money(credits.total_credits)),
        ),
        (
            "used".to_string(),
            format!("{} credits", money(credits.total_usage)),
        ),
    ];
    // A capped key runs out before the account does, so report both.
    if let Some(key) = key
        && let Some(limit) = key.limit
    {
        details.push(("key limit".to_string(), money(limit)));
        if let Some(left) = key.limit_remaining {
            details.push(("key limit remaining".to_string(), money(left)));
        }
    }
    if key.is_some_and(|key| key.is_free_tier == Some(true)) {
        details.push(("tier".to_string(), "free".to_string()));
    }
    Report {
        remaining: format!(
            "{} credits",
            money(credits.total_credits - credits.total_usage)
        ),
        details,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_providers_are_recognised_by_host() {
        assert_eq!(
            known_for("https://api.deepseek.com/v1"),
            Some(Known::DeepSeek)
        );
        assert_eq!(
            known_for("https://openrouter.ai/api/v1"),
            Some(Known::OpenRouter)
        );
        assert_eq!(known_for("http://127.0.0.1:8503/v1"), None);
        assert_eq!(known_for("https://api.openai.com/v1"), None);
    }

    #[test]
    fn endpoints_hang_off_the_origin_not_the_api_path() {
        assert_eq!(
            origin("https://api.deepseek.com/v1").unwrap(),
            "https://api.deepseek.com"
        );
        assert_eq!(
            origin("https://openrouter.ai/api/v1").unwrap(),
            "https://openrouter.ai"
        );
        assert_eq!(
            origin("http://127.0.0.1:8503/v1").unwrap(),
            "http://127.0.0.1:8503"
        );
    }

    #[test]
    fn deepseek_balance_is_read_from_the_documented_shape() {
        let balance: DeepSeekBalance = serde_json::from_str(
            r#"{
                "is_available": true,
                "balance_infos": [
                    {
                        "currency": "CNY",
                        "total_balance": "110.00",
                        "granted_balance": "10.00",
                        "topped_up_balance": "100.00"
                    }
                ]
            }"#,
        )
        .unwrap();
        let report = deepseek_report(&balance);
        assert_eq!(report.remaining, "110.00 CNY");
        assert_eq!(
            report.details,
            vec![
                ("granted".to_string(), "10.00 CNY".to_string()),
                ("topped up".to_string(), "100.00 CNY".to_string()),
                ("accepting requests".to_string(), "yes".to_string()),
            ]
        );
    }

    #[test]
    fn deepseek_reports_every_currency_it_returns() {
        let balance: DeepSeekBalance = serde_json::from_str(
            r#"{
                "is_available": false,
                "balance_infos": [
                    {"currency": "CNY", "total_balance": "0.00"},
                    {"currency": "USD", "total_balance": "5.00", "granted_balance": "5.00"}
                ]
            }"#,
        )
        .unwrap();
        let report = deepseek_report(&balance);
        assert_eq!(report.remaining, "0.00 CNY, 5.00 USD");
        assert!(
            report
                .details
                .contains(&("granted".to_string(), "5.00 USD".to_string()))
        );
        assert_eq!(
            report.details.last().unwrap(),
            &("accepting requests".to_string(), "no".to_string())
        );
    }

    #[test]
    fn openrouter_remaining_is_credits_minus_usage() {
        let credits: OpenRouterEnvelope<OpenRouterCredits> =
            serde_json::from_str(r#"{"data": {"total_credits": 10.0, "total_usage": 3.5}}"#)
                .unwrap();
        let report = openrouter_report(&credits.data, None);
        assert_eq!(report.remaining, "6.50 credits");
        assert_eq!(
            report.details,
            vec![
                ("granted".to_string(), "10.00 credits".to_string()),
                ("used".to_string(), "3.50 credits".to_string()),
            ]
        );
    }

    #[test]
    fn openrouter_adds_the_key_cap_when_the_key_has_one() {
        let credits: OpenRouterEnvelope<OpenRouterCredits> =
            serde_json::from_str(r#"{"data": {"total_credits": 10.0, "total_usage": 3.5}}"#)
                .unwrap();
        let key: OpenRouterEnvelope<OpenRouterKey> = serde_json::from_str(
            r#"{"data": {"label": "sk-or-v1-...", "usage": 3.5, "limit": 5.0,
                        "limit_remaining": 1.5, "is_free_tier": false}}"#,
        )
        .unwrap();
        let report = openrouter_report(&credits.data, Some(&key.data));
        assert_eq!(report.remaining, "6.50 credits");
        assert!(
            report
                .details
                .contains(&("key limit remaining".to_string(), "1.50".to_string()))
        );

        // An uncapped key contributes nothing but its tier.
        let uncapped: OpenRouterEnvelope<OpenRouterKey> =
            serde_json::from_str(r#"{"data": {"limit": null, "is_free_tier": true}}"#).unwrap();
        let report = openrouter_report(&credits.data, Some(&uncapped.data));
        assert!(!report.details.iter().any(|(label, _)| label == "key limit"));
        assert_eq!(
            report.details.last().unwrap(),
            &("tier".to_string(), "free".to_string())
        );
    }

    #[test]
    fn an_oversized_error_body_is_truncated() {
        let long = "x".repeat(1000);
        assert_eq!(short(&long).len(), 303);
        assert_eq!(short("  {\"error\":\"nope\"}  "), "{\"error\":\"nope\"}");
    }
}
