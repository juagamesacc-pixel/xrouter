//! Universal model addressing for request routing.
//!
//! A request's `model` field is resolved to a routing target using a 3-step
//! fallback:
//!
//! 1. **Exact tier name** → existing strict tier routing (failover *within*
//!    the tier only; no cross-tier degradation).
//! 2. **`provider/model` format** → route directly to that single endpoint.
//!    The provider must be configured; the model id is taken verbatim. There is
//!    no cross-model failover — retries only rotate keys and quota-bans still
//!    apply. This makes tiers optional: any model of any configured provider is
//!    addressable by id.
//! 3. **Bare model id** → search every configured tier entry for an exact
//!    `model` match. A unique match routes directly. An ambiguous match (the
//!    same id in 2+ providers) is rejected with the candidate options. No match
//!    falls through to the existing 404 `unknown_tier` error.

use xrouter_config::Config;
use xrouter_core::{EndpointId, ModelEntry, Tier};

/// A resolved routing target.
#[derive(Debug)]
pub enum ResolvedTarget {
    /// Exact tier-name match: full tier semantics (failover within the tier).
    Tier(Tier),
    /// Direct model addressing: a single endpoint, no cross-model failover.
    Direct(ModelEntry),
}

/// Why a `model` field could not be resolved to a routable target.
#[derive(Debug)]
pub enum ResolutionError {
    /// The same bare model id matched 2+ providers. The list is
    /// `(provider, model)` pairs the caller should surface so the user can
    /// disambiguate via the `provider/model` format.
    Ambiguous(Vec<(String, String)>),
    /// No tier or model matched.
    NotFound,
}

/// Resolve a request `model` field to a routing target.
///
/// See the module docs for the 3-step fallback. `cfg` is the live configuration.
pub fn resolve_model(cfg: &Config, model: &str) -> Result<ResolvedTarget, ResolutionError> {
    // Step 1: exact tier-name match → existing strict tier routing.
    if let Some(tier) = cfg.tiers.iter().find(|t| t.name == model) {
        return Ok(ResolvedTarget::Tier(tier.clone()));
    }

    // Step 2: `provider/model` format → route directly to that single endpoint.
    if let Some((prov, mid)) = model.split_once('/') {
        if !prov.is_empty() && !mid.is_empty() && cfg.providers.contains_key(prov) {
            return Ok(ResolvedTarget::Direct(ModelEntry {
                provider: prov.to_string(),
                model: mid.to_string(),
                is_default: true,
                weight: 1,
                endpoint_id: EndpointId::new(prov, mid),
            }));
        }
        // The provider portion is not a configured provider, so this is not a
        // valid `provider/model` address. Fall through to step 3 — the whole
        // string may be a bare model id that happens to contain a slash
        // (e.g. "black-forest-labs/FLUX.1-schnell").
    }

    // Step 3: bare model id across all configured tier entries.
    let matches: Vec<(String, String)> = cfg
        .tiers
        .iter()
        .flat_map(|t| t.entries.iter())
        .filter(|e| e.model == model)
        .map(|e| (e.provider.clone(), e.model.clone()))
        .collect();

    match matches.len() {
        0 => Err(ResolutionError::NotFound),
        1 => Ok(ResolvedTarget::Direct(ModelEntry {
            provider: matches[0].0.clone(),
            model: matches[0].1.clone(),
            is_default: true,
            weight: 1,
            endpoint_id: EndpointId::new(&matches[0].0, &matches[0].1),
        })),
        _ => Err(ResolutionError::Ambiguous(matches)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use xrouter_config::{Config, ProviderConfig, Settings};

    fn cfg_with_tiers(tiers: Vec<Tier>) -> Config {
        let mut providers = HashMap::new();
        providers.insert(
            "opencode-zen".to_string(),
            ProviderConfig {
                kind: "openai-compat".into(),
                base_url: "https://opencode.ai/zen/v1".into(),
                enabled: true,
                keys: vec![],
                quota_ban_secs: 300,
                accounts: vec![],
            },
        );
        providers.insert(
            "openrouter".to_string(),
            ProviderConfig {
                kind: "openai-compat".into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                enabled: true,
                keys: vec![],
                quota_ban_secs: 300,
                accounts: vec![],
            },
        );
        Config {
            settings: Settings::default(),
            providers,
            tiers,
        }
    }

    fn entry(provider: &str, model: &str) -> ModelEntry {
        ModelEntry {
            provider: provider.into(),
            model: model.into(),
            is_default: true,
            weight: 1,
            endpoint_id: EndpointId::new(provider, model),
        }
    }

    #[test]
    fn tier_name_match_uses_strict_tier() {
        let cfg = cfg_with_tiers(vec![Tier::new(
            "big-pickle",
            vec![entry("opencode-zen", "big-pickle")],
        )]);
        match resolve_model(&cfg, "big-pickle").unwrap() {
            ResolvedTarget::Tier(t) => assert_eq!(t.name, "big-pickle"),
            _ => panic!("expected tier target"),
        }
    }

    #[test]
    fn provider_model_format_routes_directly() {
        let cfg = cfg_with_tiers(vec![Tier::new(
            "big-pickle",
            vec![entry("opencode-zen", "big-pickle")],
        )]);
        match resolve_model(&cfg, "opencode-zen/mimo-v2.5-free").unwrap() {
            ResolvedTarget::Direct(e) => {
                assert_eq!(e.provider, "opencode-zen");
                assert_eq!(e.model, "mimo-v2.5-free");
            }
            _ => panic!("expected direct target"),
        }
    }

    #[test]
    fn bare_id_unique_match_routes_directly() {
        let cfg = cfg_with_tiers(vec![Tier::new(
            "zen-free",
            vec![entry("opencode-zen", "mimo-v2.5-free")],
        )]);
        match resolve_model(&cfg, "mimo-v2.5-free").unwrap() {
            ResolvedTarget::Direct(e) => {
                assert_eq!(e.provider, "opencode-zen");
                assert_eq!(e.model, "mimo-v2.5-free");
            }
            _ => panic!("expected direct target"),
        }
    }

    #[test]
    fn bare_id_ambiguous_is_rejected_with_options() {
        let cfg = cfg_with_tiers(vec![
            Tier::new("a", vec![entry("opencode-zen", "mimo-v2.5-free")]),
            Tier::new("b", vec![entry("openrouter", "mimo-v2.5-free")]),
        ]);
        match resolve_model(&cfg, "mimo-v2.5-free") {
            Err(ResolutionError::Ambiguous(opts)) => {
                assert_eq!(opts.len(), 2);
                assert!(opts.contains(&("opencode-zen".to_string(), "mimo-v2.5-free".to_string())));
                assert!(opts.contains(&("openrouter".to_string(), "mimo-v2.5-free".to_string())));
            }
            other => panic!("expected ambiguous, got {:?}", other),
        }
    }

    #[test]
    fn no_match_is_not_found() {
        let cfg = cfg_with_tiers(vec![Tier::new(
            "big-pickle",
            vec![entry("opencode-zen", "big-pickle")],
        )]);
        match resolve_model(&cfg, "does-not-exist") {
            Err(ResolutionError::NotFound) => {}
            other => panic!("expected not found, got {:?}", other),
        }
    }

    #[test]
    fn unknown_provider_slash_falls_through_to_bare_match() {
        // "foo/bar" where foo is not a configured provider must be treated as a
        // bare model id (which may itself contain a slash).
        let cfg = cfg_with_tiers(vec![Tier::new(
            "images",
            vec![entry("together-image", "foo/bar")],
        )]);
        match resolve_model(&cfg, "foo/bar").unwrap() {
            ResolvedTarget::Direct(e) => {
                assert_eq!(e.provider, "together-image");
                assert_eq!(e.model, "foo/bar");
            }
            _ => panic!("expected direct target via bare match"),
        }
    }
}
