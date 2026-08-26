//! Universal model addressing for request routing.
//!
//! A request's `model` field is resolved to a routing target using a layered
//! fallback that copes with every id format a user might copy from a built-in
//! list, a provider's `/models` response, or another tool:
//!
//! 1. **Exact tier name** → existing strict tier routing (failover *within*
//!    the tier only; no cross-tier degradation).
//! 2. **`provider/model` format** → route directly to that single endpoint.
//!    The provider must be configured; the model id is taken verbatim. There is
//!    no cross-model failover — retries only rotate keys and quota-bans still
//!    apply. This makes tiers optional: any model of any configured provider is
//!    addressable by id (on-demand routing).
//!    - If the provider portion is *not* a configured provider, the whole
//!      string is treated as a bare model id below (this is how OpenRouter-style
//!      ids like `cohere/north-mini-code:free` resolve: `cohere` is not an
//!      xrouter provider, but `openrouter` has that exact id in its model
//!      cache, so we route to `openrouter` with the full id).
//! 3. **Bare model id** (no configured provider prefix) → search every
//!    configured tier entry's `model` field for an exact match, then search the
//!    on-demand model cache (`known`) across all configured providers. A unique
//!    match routes directly; an ambiguous match (same id in 2+ providers) is
//!    rejected with the candidate options.
//! 4. **`:free` / `-free` alias** → if the bare search fails and the id ends
//!    with a free suffix, strip it and retry the bare search (so
//!    `cohere/north-mini-code:free` still resolves when the cache stores the
//!    id without the suffix, and vice-versa).

use xrouter_config::Config;
use xrouter_core::{EndpointId, ModelEntry, Tier};
use xrouter_providers::ModelCache;
use tracing::debug;

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
/// See the module docs for the layered fallback. `cfg` is the live
/// configuration; `known` is the optional on-demand model cache (populated by
/// the server's `refresh_model_cache`). When `None`, only tier entries and
/// configured-provider `provider/model` addressing are considered.
pub fn resolve_model(
    cfg: &Config,
    model: &str,
    known: Option<&ModelCache>,
) -> Result<ResolvedTarget, ResolutionError> {
    let raw = model.trim();
    if raw.is_empty() {
        return Err(ResolutionError::NotFound);
    }

    // Step 1: exact tier-name match → existing strict tier routing.
    if let Some(tier) = cfg.tiers.iter().find(|t| t.name == raw) {
        return Ok(ResolvedTarget::Tier(tier.clone()));
    }

    // Step 2: `provider/model` format → route directly to that single endpoint.
    if let Some((prov, mid)) = raw.split_once('/') {
        if !prov.is_empty() && !mid.is_empty() {
            if cfg.providers.contains_key(prov) {
                // On-demand direct routing to a configured provider. The model
                // id is taken verbatim; the upstream provider validates it.
                return Ok(ResolvedTarget::Direct(ModelEntry {
                    provider: prov.to_string(),
                    model: mid.to_string(),
                    is_default: true,
                    weight: 1,
                    endpoint_id: EndpointId::new(prov, mid),
                }));
            }
            // The provider portion is not a configured provider, so this is not
            // a valid `provider/model` address. Fall through to step 3 — the
            // whole string may be a bare model id that happens to contain a
            // slash (e.g. "cohere/north-mini-code:free" from OpenRouter's
            // /models list, where `cohere` is not an xrouter provider but
            // `openrouter` knows the full id).
        }
    }

    // Step 3 + 4: bare model id search, with `:free`/`-free` suffix stripping.
    resolve_bare(cfg, raw, known)
}

/// Search tier entries and the on-demand cache for a bare model id, applying a
/// `:free`/`-free` suffix-strip fallback when the first pass finds nothing.
fn resolve_bare(
    cfg: &Config,
    model: &str,
    known: Option<&ModelCache>,
) -> Result<ResolvedTarget, ResolutionError> {
    let candidates = collect_bare_candidates(cfg, model, known);
    if !candidates.is_empty() {
        return pick(candidates);
    }

    // Step 4: `:free` / `-free` alias handling.
    let stripped = strip_free_suffix(model);
    if stripped != model {
        let candidates = collect_bare_candidates(cfg, &stripped, known);
        if !candidates.is_empty() {
            return pick(candidates);
        }
    }

    Err(ResolutionError::NotFound)
}

/// Gather every `(provider, model)` candidate for a bare id from both tier
/// entries and the on-demand model cache. Results are de-duplicated.
fn collect_bare_candidates(
    cfg: &Config,
    model: &str,
    known: Option<&ModelCache>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();

    // (a) tier entries: exact `model` match.
    for t in &cfg.tiers {
        for e in &t.entries {
            if e.model == model {
                out.push((e.provider.clone(), e.model.clone()));
            }
        }
    }

    // (b) on-demand cache: any configured, enabled provider that lists this id.
    if let Some(cache) = known {
        for (prov, pcfg) in &cfg.providers {
            if !pcfg.enabled {
                continue;
            }
            // `get_or_stale` returns the cached list even if it has expired (TTL
            // passed). We keep serving it for availability, but log so operators
            // can see we're routing on stale model metadata. A proper async
            // refresh (background `refresh_model_cache`) is the long-term fix;
            // this call stays blocking to keep resolution allocation-free.
            if !cache.is_fresh(prov) {
                debug!(provider = prov, "serving stale model cache for resolution");
            }
            if let Some(models) = cache.get_or_stale(prov) {
                for m in models {
                    if m.id == model {
                        out.push((prov.clone(), m.id.clone()));
                    }
                }
            }
        }
    }

    out.sort();
    out.dedup();
    out
}

/// Build a direct target from a single candidate, or report ambiguity.
fn pick(candidates: Vec<(String, String)>) -> Result<ResolvedTarget, ResolutionError> {
    match candidates.len() {
        0 => Err(ResolutionError::NotFound),
        1 => Ok(ResolvedTarget::Direct(ModelEntry {
            provider: candidates[0].0.clone(),
            model: candidates[0].1.clone(),
            is_default: true,
            weight: 1,
            endpoint_id: EndpointId::new(&candidates[0].0, &candidates[0].1),
        })),
        _ => Err(ResolutionError::Ambiguous(candidates)),
    }
}

/// Strip a free-model marker from `model`, mirroring the patterns recognized by
/// `xrouter_core::is_free` (case-insensitive). A cached id stored *without* the
/// marker still resolves when the user sends it *with* the marker, and
/// vice-versa.
///
/// Markers handled (longest/most-specific first so e.g. `-free-` is preferred
/// over `-free`): `[free]-`, `(free)-`, `-free-`, `:free`, `-free`, `[free]`,
/// `(free)`. The first marker found (case-insensitively) is removed; if none is
/// present the input is returned unchanged.
fn strip_free_suffix(model: &str) -> String {
    // Keep this list in sync with `xrouter_core::is_free`.
    let markers = [":free", "-free-", "[free]-", "(free)-", "-free", "[free]", "(free)"];
    let lower = model.to_lowercase();
    for m in markers {
        if let Some(pos) = lower.find(m) {
            let mut s = model.to_string();
            // Remove the marker at the located position (length is ASCII-stable
            // because the marker is ASCII and the match is case-insensitive).
            s.replace_range(pos..pos + m.len(), "");
            return s;
        }
    }
    model.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use xrouter_config::{Config, ProviderConfig, Settings};
    use xrouter_core::is_free;
    use xrouter_providers::ModelCache;

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
        providers.insert(
            "together-image".to_string(),
            ProviderConfig {
                kind: "openai-compat".into(),
                base_url: "https://api.together.ai/v1".into(),
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

    /// A temp-backed model cache so tests never touch the real ~/.cache.
    fn temp_cache() -> ModelCache {
        let dir = std::env::temp_dir().join(format!("xrouter-resolve-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        ModelCache::new(dir.join("models.json"))
    }

    #[test]
    fn tier_name_match_uses_strict_tier() {
        let cfg = cfg_with_tiers(vec![Tier::new(
            "big-pickle",
            vec![entry("opencode-zen", "big-pickle")],
        )]);
        match resolve_model(&cfg, "big-pickle", None).unwrap() {
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
        match resolve_model(&cfg, "opencode-zen/mimo-v2.5-free", None).unwrap() {
            ResolvedTarget::Direct(e) => {
                assert_eq!(e.provider, "opencode-zen");
                assert_eq!(e.model, "mimo-v2.5-free");
            }
            _ => panic!("expected direct target"),
        }
    }

    #[test]
    fn provider_model_format_with_builtin_tier_name() {
        // `opencode-zen/big-pickle` is both a tier name (big-pickle) and a
        // provider/model id; the provider/model form must route directly.
        let cfg = cfg_with_tiers(vec![Tier::new(
            "big-pickle",
            vec![entry("opencode-zen", "big-pickle")],
        )]);
        match resolve_model(&cfg, "opencode-zen/big-pickle", None).unwrap() {
            ResolvedTarget::Direct(e) => {
                assert_eq!(e.provider, "opencode-zen");
                assert_eq!(e.model, "big-pickle");
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
        match resolve_model(&cfg, "mimo-v2.5-free", None).unwrap() {
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
        match resolve_model(&cfg, "mimo-v2.5-free", None) {
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
        match resolve_model(&cfg, "does-not-exist", None) {
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
        match resolve_model(&cfg, "foo/bar", None).unwrap() {
            ResolvedTarget::Direct(e) => {
                assert_eq!(e.provider, "together-image");
                assert_eq!(e.model, "foo/bar");
            }
            _ => panic!("expected direct target via bare match"),
        }
    }

    #[test]
    fn bare_id_resolves_from_on_demand_cache() {
        // A fetched model that is NOT in any tier still resolves via the cache.
        let cfg = cfg_with_tiers(vec![]);
        let cache = temp_cache();
        cache.insert(
            "opencode-zen".to_string(),
            vec![xrouter_providers::RawModel {
                id: "mimo-v2.5-free".into(),
                name: None,
            }],
        );
        match resolve_model(&cfg, "mimo-v2.5-free", Some(&cache)).unwrap() {
            ResolvedTarget::Direct(e) => {
                assert_eq!(e.provider, "opencode-zen");
                assert_eq!(e.model, "mimo-v2.5-free");
            }
            _ => panic!("expected direct target from cache"),
        }
    }

    #[test]
    fn provider_prefixed_id_resolves_via_cache_when_prefix_not_a_provider() {
        // OpenRouter-style id: `cohere/north-mini-code:free`. `cohere` is not a
        // configured provider, but `openrouter` lists the full id in its cache.
        let cfg = cfg_with_tiers(vec![]);
        let cache = temp_cache();
        cache.insert(
            "openrouter".to_string(),
            vec![xrouter_providers::RawModel {
                id: "cohere/north-mini-code:free".into(),
                name: None,
            }],
        );
        match resolve_model(&cfg, "cohere/north-mini-code:free", Some(&cache)).unwrap() {
            ResolvedTarget::Direct(e) => {
                assert_eq!(e.provider, "openrouter");
                assert_eq!(e.model, "cohere/north-mini-code:free");
            }
            _ => panic!("expected direct target via cache"),
        }
    }

    #[test]
    fn free_suffix_strip_fallback_resolves() {
        // Cache stores the id WITHOUT the :free suffix; the user sends it WITH.
        let cfg = cfg_with_tiers(vec![]);
        let cache = temp_cache();
        cache.insert(
            "openrouter".to_string(),
            vec![xrouter_providers::RawModel {
                id: "cohere/north-mini-code".into(),
                name: None,
            }],
        );
        match resolve_model(&cfg, "cohere/north-mini-code:free", Some(&cache)).unwrap() {
            ResolvedTarget::Direct(e) => {
                assert_eq!(e.provider, "openrouter");
                assert_eq!(e.model, "cohere/north-mini-code");
            }
            _ => panic!("expected direct target via :free strip"),
        }
    }

    #[test]
    fn cache_match_ambiguous_across_providers() {
        let cfg = cfg_with_tiers(vec![]);
        let cache = temp_cache();
        cache.insert(
            "opencode-zen".to_string(),
            vec![xrouter_providers::RawModel {
                id: "shared-model".into(),
                name: None,
            }],
        );
        cache.insert(
            "openrouter".to_string(),
            vec![xrouter_providers::RawModel {
                id: "shared-model".into(),
                name: None,
            }],
        );
        match resolve_model(&cfg, "shared-model", Some(&cache)) {
            Err(ResolutionError::Ambiguous(opts)) => assert_eq!(opts.len(), 2),
            other => panic!("expected ambiguous, got {:?}", other),
        }
    }

    /// `strip_free_suffix` must invert `is_free`: every id `is_free` recognizes
    /// strips down to a non-free id, and non-free ids are left untouched.
    #[test]
    fn free_strip_inverts_is_free() {
        let free = [
            "mimo-v2.5-free",
            "deepseek/deepseek-r1:free",
            "qwen3-coder-[free]",
            "some-model(free)",
            "model-[free]-v2",
            "MODEL-FREE",
        ];
        for f in free {
            let stripped = strip_free_suffix(f);
            assert!(is_free(f), "precondition: {} should be free", f);
            assert!(!is_free(&stripped), "strip({}) = '{}' should NOT be free", f, stripped);
        }
        // Non-free ids must pass through unchanged.
        for nf in ["gpt-4", "claude-sonnet-4", "freewheel", "cohere/north-mini-code"] {
            assert!(!is_free(nf));
            assert_eq!(strip_free_suffix(nf), nf, "non-free id {} must be unchanged", nf);
        }
    }
}
