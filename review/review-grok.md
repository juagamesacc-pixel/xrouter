> **Agent Instruction — Discernment Required**
> Use your own discernment and self-prove that each suggested review are valid and if there are limitations then it must self-prove whether fixing the limitation is worth it or not and if it has dilemma then ask user(not directly but specify what happens if it fix or not fix the limitations).

**xrouter** is a multi-crate Rust LLM router (OpenAI-compatible + Anthropic ingress) with tier-based model aliases, multi-provider failover, key rotation, basic circuit-breaking, config hot-reload, and protocol translation.

I reviewed the actual sources + `Cargo.toml`s (no README). ~2.4k LOC across the workspace.

### Architecture
Clean, sensible split:

| Crate | Role |
|-------|------|
| `xrouter-core` | Types (`ProviderId`, `ApiKey`, `Protocol`, `Tier`/`ModelEntry`/`TierRegistry`), errors, free-model heuristic |
| `xrouter-config` | TOML load/save (atomic + 0600), defaults, `notify`-based watcher with 200 ms debounce |
| `xrouter-providers` | `Provider` trait, OpenAI-compat + Anthropic adapters, `ModelCache` (DashMap + disk snapshot, 10 min TTL) |
| `xrouter-balancer` | Per-provider `KeyRing` (atomic RR), `HealthRegistry` (Healthy / Cooling / HalfOpen), dead-key set, candidate ordering |
| `xrouter-server` | Axum routes, routing loop, request/response + SSE translation, metrics |
| `xrouter-cli` | Wizard, `serve`, keys/models/tier management, live test |

Workspace deps are recent and consistent (tokio 1.47, axum 0.7, reqwest with rustls, dashmap, notify, etc.).

### What works well
- **Tier model is clear and strict**: `model` field is a tier name; no silent cross-tier fallback. Exhausted tiers return a well-defined error body (OpenAI or Anthropic shape).
- **Resilience primitives are present**:
  - Key RR + dead-key skip on 401/403
  - Endpoint cooling after 3 failures with exponential backoff (capped 300 s) + `Retry-After` support
  - Provider-level degradation (>70 % cooling) deprioritizes entries inside the tier
  - Attempt cap (`min(candidates × keys, 6)`)
  - Per-attempt 15 s timeout + jitter on 5xx
- **Streaming path is correct when protocols match**: real `bytes_stream()` passthrough with proper SSE headers.
- **Config hygiene**: atomic rename, mode 0600, hot-reload updates both shared `Config` and balancer keyrings.
- **Model cache**: in-memory + persisted, used by `/admin/models` and CLI so live list calls are not hammered.
- **Panic isolation** around translation and reload.
- **Basic observability**: atomic counters → Prometheus text + JSON `/admin/stats`.
- Unit tests cover the important bits in core/balancer/translate/cache.

### Issues / incomplete pieces
1. **Weighted selection is a stub**  
   `pick_weighted` just returns the max-weight candidate; the real path uses `candidates_ordered`. Comment admits “for simplicity”. Weights are stored but effectively ignored for load distribution.

2. **Streaming translation is lossy and forces buffering**  
   When ingress ≠ upstream protocol the whole body is collected, then a minimal line-by-line rewrite runs. Tool-call deltas, proper `message_start` / `content_block_start`, usage, etc. are missing or incomplete. Non-stream translation is also simplified (text-only extraction).

3. **Graceful shutdown is broken on Unix**  
   `axum::serve` is moved into a `select!` with signals; on SIGINT/SIGTERM the future is dropped after a 500 ms sleep. The leftover comment acknowledges the problem. No proper drain / with_graceful_shutdown.

4. **Admin surface is open**  
   `/admin/*` has no auth even when `settings.api_token` is set. Only the chat endpoints check the Bearer token.

5. **Lock hygiene**  
   Multiple `config.write().unwrap()` / `read().unwrap()`. Fine for a single-threaded config path, but a poisoned lock would panic the worker.

6. **Half-open is only partially wired**  
   `maybe_half_open` exists and is called, but success/failure transitions and probe semantics are coarse.

7. **Unused / dead fields**  
   `ProviderConfig.rr_cursor` is never used (balancer owns its own atomic). `default_entry` on `Tier` is stored but not consulted in the routing loop.

8. **CLI / wizard**  
   Functional but linear; Anthropic `list_models` always returns empty (correct per API, but the UX doesn’t special-case it). Wizard still creates OpenAI-compat adapters for validation even for Anthropic.

9. **Tooling / lockfile**  
   `Cargo.lock` is version 4; the sandbox has cargo 1.75 which cannot parse it. Workspace pins are very recent — expect friction on older toolchains.

10. **Missing production-oriented features**  
    No request-id / correlation, no concurrency / rate-limit middleware, no upstream connection pooling beyond reqwest defaults, no structured error mapping for every status, limited tracing spans.

### Code quality notes
- Readable, mostly idiomatic.
- Good use of `DashMap`, `Arc`, atomics, `async_trait`.
- Translation helpers are a bit duplicated between request path and stream path.
- `peek_model` still does a full `serde_json` parse (the “fast” claim is aspirational).
- Error bodies are protocol-aware, which is nice.

### Verdict
Solid early-stage design. The crate boundaries, strict-tier semantics, key + health machinery, and hot-reload are the strong parts. The main gaps are incomplete weighted balancing, incomplete cross-protocol streaming fidelity, broken graceful shutdown, and open admin endpoints.

If you keep going, the highest-leverage next steps would be:

1. Real smooth WRR (or at least weighted RR) state in the balancer.
2. Proper streaming translators (or document that cross-protocol streaming is best-effort / buffered).
3. Fix shutdown with `axum::serve(...).with_graceful_shutdown`.
4. Protect `/admin/*` with the same token (or a separate admin token).
5. Integration tests with `wiremock` for the full routing + retry loop.

Happy to dive deeper into any specific crate or suggest concrete patches.
