# 05 — Tiers: Strict Model Routing (R8, R11)

## Concept

Users select multiple models during wizard setup and group them into named tiers. The tier name is what clients put in the request's `model` field:

```json
POST /v1/chat/completions
{ "model": "big-pickle", "messages": [...] }
```

`big-pickle` here is a **tier alias**, not a literal upstream model. A tier contains one or more concrete `(provider, model)` entries — possibly spanning providers.

## Resolution rules (STRICT)

1. Request `model` field must match a tier name exactly. No fuzzy matching, no prefix matching.
2. Candidates = that tier's entries **only**.
3. Failover walks candidates within the tier (ordered, then health-aware RR). Never touches another tier.
4. If all candidates + all keys on the tier are exhausted/unhealthy:
   - Return `503` with structured body:
     ```json
     { "error": { "type": "tier_exhausted", "tier": "big-pickle",
                  "message": "all models/providers for tier 'big-pickle' failed" } }`
     ```
   - **No degrade to a cheaper/lower tier. No upgrade to a stronger tier. Ever.**
5. Unknown tier name → `404 unknown_tier` listing available tiers (helpful, still strict).

## Tier shape

```toml
[[tiers]]
name = "smart"
strict = true            # always true; field exists for explicitness/future proofing
default_entry = 0        # used when client omits preference inside tier
entries = [
  { provider = "opencode-zen", model = "big-pickle" },
  { provider = "openrouter",   model = "anthropic/claude-sonnet-4" },
]
```

## Why strict-only

Tier names are contracts for agent workflows (e.g., Claude Code "model" setting). Silent tier substitution produces wrong cost/quality characteristics and hides outages. xrouter fails loudly instead (R11).

## Management

- `xrouter tier add <name> ...` / wizard step 4 create tiers.
- `GET /admin/tiers` lists tiers with live health per entry.
- Renaming a tier is allowed; deleting a tier that received traffic requires `--force`.
