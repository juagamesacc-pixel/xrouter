# 07 — Performance Engineering ("fastest router")

## Latency budget (hot path, non-streaming TTFB)

| Stage | Budget |
|---|---|
| Axum parse + peek model field | <100µs |
| Tier resolve + key RR pick | <1µs (atomics + small vec scan) |
| Protocol translation (if needed) | <200µs typical payload |
| Conn acquire from pool | <50µs warm |
| **Router overhead total** | **<0.5ms** vs raw upstream call |

## Techniques

1. **Two-phase parse**: read only enough of the body to extract `model` (tier key), then route. Full deserialization only when translation is required; when ingress protocol == upstream protocol, forward the original bytes verbatim (zero re-serialization).
2. **Connection pooling**: `hyper-util` client with per-provider pools, HTTP/2 multiplexing where upstream supports it, TCP_NODELAY, keep-alive forever.
3. **Lock-free hot path**: round-robin cursors are atomics; health map is sharded (`DashMap`); no mutexes touched per request in the happy path.
4. **Streaming zero-copy**: SSE bytes piped through `tokio` channels of `Bytes`; never accumulate full responses.
5. **Allocation discipline**: precomputed auth header values per key (`HeaderName`/`HeaderValue` cached); `SmallVec` for candidate lists; avoid `String` clones in resolve.
6. **TLS**: rustls with session resumption; one `Client` per provider config reused across tasks.
7. **Runtime**: tokio multi-threaded, `SO_REUSEPORT` option to run N processes pinned across cores behind a shared socket (optional mode for extreme throughput).
8. **No logging on hot path**: tracing spans sampled; metrics via atomic counters exported on `/admin/stats`.

## Benchmarks (acceptance)

- `cargo bench` criterion suite: resolve(), KeyRing::next(), translation round-trips.
- wrk/k6 load test: ≥50k RPS routing overhead-only on a dev box; p99 router-added latency <1ms at 10k concurrent streams.

## What we deliberately do NOT do

- No request queuing/prioritization logic (adds latency).
- No response caching of completions (correctness risk, rarely useful for agents).
- No regex/matching on tier names at runtime (exact HashMap lookup only).
