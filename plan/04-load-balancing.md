# 04 — Load Balancing & Key Round-Robin

## Key round-robin (R4)

Per provider, an `AtomicUsize` cursor over the key list:

```rust
pub struct KeyRing {
    keys: Arc<[ApiKey]>,
    cursor: AtomicUsize,
}
impl KeyRing {
    fn next(&self) -> &ApiKey {
        let i = self.cursor.fetch_add(1, Ordering::Relaxed) % self.keys.len();
        &self.keys[i]
    }
}
```

- Every request to the same provider uses the **next** key — strict per-request rotation.
- Keys marked unhealthy (401/403/429) are skipped by the health layer but keep their slot; rotation order stays stable.
- Adding/removing keys at runtime swaps the `Arc<[ApiKey]>` atomically (arc-swap); cursor resets safely.

## Candidate selection within a tier

A tier's `entries` list is ordered. Selection:

1. Filter entries whose endpoint is healthy.
2. Among healthy entries, pick via smooth weighted round-robin (weights default 1; configurable per entry).
3. Within the chosen entry's provider, `KeyRing::next()` supplies the key.

## Health model

State machine per `(provider, model)` endpoint:

```
Healthy ──(failure threshold: 3 consecutive OR 2 in 5s)──▶ Cooling(30s, exp backoff ×2, cap 5min)
Cooling ──(timer)──▶ HalfOpen ──(probe success)──▶ Healthy
HalfOpen ──(probe fail)──▶ Cooling
```

- 429 → honor `Retry-After`, cool only that key if provider signals key-scoped limits, else the endpoint.
- 401/403 → mark that **key** dead until config reload.
- 5xx / network error / timeout → endpoint failure count++ and immediate retry on next candidate (R10).

All state in `DashMap<EndpointId, EndpointHealth>` — no global locks on hot path.

## Retry policy (same tier only)

| Param | Default |
|---|---|
| Max attempts across tier | `entries.len() * keys_per_provider` capped at 6 |
| Per-attempt timeout | connect 3s, first-byte 15s, idle-stream 30s |
| Backoff between candidates | 0ms (failover should be instant), jittered 0–25ms |
| Retryable | connect error, timeout, 408/429/5xx |
| Non-retryable | 400/401/403/404/422 (bad request or dead key — skip candidate, no replay of streamed bytes) |

If a response already started streaming and then breaks mid-stream, do **not** silently restart; emit upstream-style error event (clients handle resumption).
