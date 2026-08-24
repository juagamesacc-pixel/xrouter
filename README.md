# xrouter — blazing-fast LLM request router (Rust)

Exposes both Anthropic (`POST /v1/messages`) and OpenAI (`POST /v1/chat/completions`) ingress on one port, routing by **strict tier aliases** to upstream providers.

- Round-robin per-provider key rotation (`R4`)
- Health-aware failover within tier only (`R10`, `R11`)
- OpenAI ↔ Anthropic translation (request + streaming SSE)
- Hot-reload config (`~/.config/xrouter/config.toml`) via `notify`
- Model discovery with free-filter (`[free]`, `(free)`, `-free`, `:free`)
- Built-ins: `opencode-zen` (`https://opencode.ai/zen/v1`, default `big-pickle`) and `openrouter` (`https://openrouter.ai/api/v1`)

## Quick start

```bash
xrouter wizard          # interactive setup
xrouter serve --port 3000
curl http://127.0.0.1:3000/v1/chat/completions -H "Content-Type: application/json" -d '{"model":"big-pickle","messages":[{"role":"user","content":"hi"}]}'
curl http://127.0.0.1:3000/v1/messages -H "Content-Type: application/json" -d '{"model":"big-pickle","messages":[{"role":"user","content":"hi"}],"max_tokens":100}'
```

## Config

`~/.config/xrouter/config.toml` (0600) — see `plan/02-config-and-wizard.md` and `examples/config.toml`.

Model cache: `~/.cache/xrouter/models.json` (TTL 10 min, stale fallback).

## Endpoints

| Method | Path | Description |
|---|---|---|
| POST | `/v1/chat/completions` | OpenAI ingress |
| POST | `/v1/messages` | Anthropic ingress |
| GET | `/v1/models` | tiers as pseudo-models |
| GET | `/healthz` | liveness |
| GET | `/admin/tiers` | tier health matrix |
| GET | `/admin/models?provider=&free=` | live/cached models |
| POST | `/admin/reload` | reload config |
| GET | `/admin/metrics` | Prometheus text |
| GET | `/admin/stats` | JSON stats |

## CLI

```
xrouter wizard
xrouter serve [--host 127.0.0.1] [--port 3000]
xrouter keys add <provider>
xrouter models list [--provider P] [--free]
xrouter tier add <name> --provider P --model M
xrouter tier list
xrouter test <tier> --allow-live [--base-url http://127.0.0.1:3000]
```

## Crates

- `xrouter-core` — `Tier`, `ModelEntry`, `is_free`, error taxonomy
- `xrouter-config` — TOML load/save (atomic 0600), watcher
- `xrouter-providers` — `OpenAiCompatAdapter`, `AnthropicAdapter`, `ModelCache`
- `xrouter-balancer` — `KeyRing` (AtomicUsize), `HealthRegistry` (DashMap), WRR
- `xrouter-server` — axum + hyper-util, translation, failover, metrics
- `xrouter-cli` — clap + dialoguer wizard

## Performance

Hot path: peek model → tier resolve (HashMap) → atomic RR → pooled reqwest → zero-copy SSE passthrough. See `plan/07-performance.md`.

## Resilience

Key quarantine (401/403), endpoint circuit breaker (30s→5min exp backoff, half-open), provider degraded (>70% cooling), tier strict failover (max 6 attempts, Retry-After respected), graceful drain, panic isolation.

## License

MIT
