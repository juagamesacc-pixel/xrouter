# 01 — Architecture

## Crate layout (Cargo workspace)

```
xrouter/
├── Cargo.toml                # workspace
├── crates/
│   ├── xrouter-core/         # domain types, tiers, routing state machine
│   ├── xrouter-config/       # config load/save, wizard backend
│   ├── xrouter-providers/    # provider adapters (openai-compat, anthropic)
│   ├── xrouter-balancer/     # round-robin, health, circuit breakers
│   ├── xrouter-server/       # axum HTTP server, protocol translation (bin)
│   └── xrouter-cli/          # bin: `xrouter` — wizard, serve, models, test
└── plan/
```

## Module responsibilities

### xrouter-core
- `Tier`, `ModelEntry`, `ProviderId`, `ApiKey` value types.
- `RouteRequest { tier_alias, payload_kind }` → resolution logic lives here as pure functions (unit-testable without IO).
- Error taxonomy: `NoKeys`, `TierExhausted`, `UpstreamError(status)`, `Timeout`.

### xrouter-config
- Reads/writes `~/.config/xrouter/config.toml` (see 02).
- Hot-reload via file watcher (`notify` crate) so wizard edits apply without restart.

### xrouter-providers
- Trait `Provider`:
  ```rust
  #[async_trait]
  pub trait Provider: Send + Sync {
      async fn list_models(&self, key: &ApiKey) -> Result<Vec<RawModel>>;
      async fn send(&self, ctx: &RequestCtx) -> UpstreamResult;
      fn protocol(&self) -> Protocol; // OpenAiCompat | Anthropic
  }
  ```
- Two built-in adapter families:
  - `OpenAiCompatAdapter` — works for openrouter, opencode-zen, and any user-added `/v1/chat/completions` provider.
  - `AnthropicAdapter` — for native Anthropic API endpoints.
- Translation layer lives in server crate; adapters speak native upstream wire format.

### xrouter-balancer
- Per-provider atomic round-robin cursor over its key list (R4).
- Per-endpoint health tracking: consecutive failures, cooldowns, half-open probes (R5, see 08).
- Lock-free where possible: `AtomicUsize` cursors, `DashMap` for endpoint state.

### xrouter-server (axum + hyper, HTTP/2 enabled)
- Ingress routes: `/v1/messages`, `/v1/chat/completions`, plus management endpoints (`/admin/*`).
- Streaming passthrough using `hyper::Body` / `tokio::io` — never buffer full SSE responses.
- Request-scoped timeout + cancellation propagation to upstream.

### xrouter-cli
- Subcommands: `wizard`, `serve`, `models list/add`, `tier add/list`, `test <tier>`, `keys add`.

## Data flow (hot path)

```
client → axum handler
       → parse minimal envelope (model field only)      [fast path]
       → TierRouter.resolve(alias) → ordered candidate list
       → Balancer.pick(candidate) → (provider, key)     [atomic RR]
       → ProviderAdapter.send()                          [pooled conn]
       → stream bytes back to client                     [zero-copy passthrough]
       → on failure: mark unhealthy, advance candidate, retry (same tier only)
```

The model/tier lookup happens **before** full body deserialization: peek the small JSON envelope for `model`, resolve route, then pipe the raw body through translation only when protocols differ (see 07).
