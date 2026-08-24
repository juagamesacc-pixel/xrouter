# 08 — Resilience

## Layers (defense in depth)

1. **Key level** — invalid keys (401/403) quarantined; 429 honors `Retry-After` per key.
2. **Endpoint level** — circuit breaker per `(provider, model)`; exponential cooldown 30s→5min; half-open probes with a single cheap request before restoring traffic.
3. **Provider level** — if >70% of a provider's endpoints are cooling, provider marked degraded; tier selection prefers healthy providers first but still stays within tier.
4. **Tier level failover (R10)** — ordered candidate walk across entries in the same tier, including cross-provider alternates the user opted into during wizard ("if opencode-zen fails try openrouter").
5. **Router level** — graceful shutdown drains streams; config hot-reload never drops in-flight requests; panic isolation via `catch_unwind` around adapter calls so one bad adapter can't kill the worker.

## Timeouts & cancellation

| Timeout | Value | Behavior |
|---|---|---|
| Connect | 3s | counts as attempt failure → next candidate |
| TTFB | 15s | abort + failover |
| Stream idle | 30s | abort stream, emit error event |
| Total (non-stream) | 120s cap | configurable |

Client disconnects propagate: upstream request aborted immediately (`tokio::select!` on inbound close), freeing pool connections.

## Observability for resilience

- `/admin/tiers` shows live health matrix (endpoint × state × last error).
- Structured tracing (`tracing` + JSON subscriber option): every attempt logs `tier, provider, model, key_idx, status, latency_ms`.
- Failure counters exported as Prometheus text on `/admin/metrics`.

## Chaos acceptance tests

- Kill upstream mid-stream → client sees clean SSE error event, router recovers.
- All keys of primary provider return 429 → traffic shifts to alternate provider in same tier within one retry cycle.
- Entire tier down → strict 503 `tier_exhausted`, no cross-tier leakage (asserted in integration test).
- Config file edited while serving → reload applies without dropping requests.
