# 06 — HTTP API Surface

## Ingress endpoints (xrouter server)

| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/chat/completions` | OpenAI-compatible ingress (R1) |
| POST | `/v1/messages` | Anthropic-compatible ingress (R1) |
| GET  | `/v1/models` | list tiers as pseudo-models (so SDK model pickers work) |
| GET  | `/healthz` | liveness |
| GET  | `/admin/tiers` | tier + endpoint health snapshot |
| GET  | `/admin/models?provider=&free=` | live/cached provider models, free filter |
| POST | `/admin/reload` | force config reload |

Both ingress endpoints accept requests targeting **any** configured tier; the ingress protocol and the upstream protocol are independent (translation layer bridges them). E.g., a Claude Code client hitting `/v1/messages` with `model: "big-pickle"` can be served by an OpenAI-compatible opencode-zen upstream.

## Streaming

- Ingress SSE passthrough: `text/event-stream`, chunked.
- Cross-protocol stream translation is a chunk-wise state machine:
  - OpenAI→Anthropic shape: synthesize `message_start`, map each delta to `content_block_delta`, end with `message_stop`.
  - Anthropic→OpenAI shape: emit `chat.completion.chunk` frames.
- First byte latency target: upstream TTFB + <2ms router overhead.

## Error mapping

| Router condition | OpenAI-style body | Anthropic-style body |
|---|---|---|
| Unknown tier | 404 `unknown_tier` | 404 `not_found_error` |
| Tier exhausted (R11) | 503 `tier_exhausted` | 529/503 `overloaded_error` w/ detail |
| All keys invalid | 401 `no_valid_keys` | 401 `authentication_error` |
| Upstream timeout after failover | 504 `upstream_timeout` | 504 |

Error bodies follow the ingress protocol's native error schema so stock SDKs surface them cleanly.

## Auth on the router itself (optional)

`[settings] api_token = "..."` — if set, ingress requires `Authorization: Bearer`. Off by default for localhost use.
