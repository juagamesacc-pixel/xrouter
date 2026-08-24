# 03 — Providers & Free-Model Discovery

## Built-in default providers (R6)

| id | protocol | base_url | notes |
|---|---|---|---|
| `opencode-zen` | openai-compat | `https://opencode.ai/zen/v1` | default model: **big-pickle** |
| `openrouter` | openai-compat | `https://openrouter.ai/api/v1` | free models exposed via `:free` suffix |

Both ship pre-registered in defaults; they appear in the wizard even before any key exists. User-added providers may be `openai-compat` or `anthropic`.

## Protocol adapters

### OpenAI-compatible adapter
- `GET {base}/models` → discovery.
- `POST {base}/chat/completions` → inference; supports SSE streaming (`stream: true`).
- Auth: `Authorization: Bearer <key>`.

### Anthropic adapter
- `POST {base}/v1/messages`, headers `x-api-key`, `anthropic-version`.
- Streaming via SSE `message_start/content_block_delta/...`.

Translation matrix lives in `xrouter-server::translate`:

| Direction | Conversion |
|---|---|
| OpenAI req → Anthropic upstream | `messages[].role/content` reshape, `max_tokens` required-default, system extraction |
| Anthropic req → OpenAI upstream | system → first message, tool mapping, stop sequences ↔ `stop` |
| Streaming | SSE event kinds translated chunk-by-chunk (state machine, no buffering) |

## Free-model filtering (R7)

A model is **free** iff its id/name matches any of:

```rust
fn is_free(name: &str) -> bool {
    let n = name.to_lowercase();
    // bracketed forms: "[free]", "(free)"
    n.contains("[free]") || n.contains("(free)")
    // suffix form: "mimo-v2.5-free", "deepseek-r1:free"
    || n.ends_with("-free") || n.ends_with(":free")
    || n.contains("-free-") || n.contains("[free]-") || n.contains("(free)-")
}
```

Examples matched: `[free]`, `(free)`, `mimo-v2.5-free`, `deepseek/deepseek-r1:free`.

Wizard shows only free models by default (toggle to show all). The filter is also available at runtime: `GET /admin/models?provider=opencode-zen&free=true`.

## Model cache

- Discovery results cached in-memory per provider with TTL (10 min) + persisted snapshot in `~/.cache/xrouter/models.json` so wizard works offline with stale data (clearly labeled).
- Cache refresh is background; hot path never blocks on discovery.

## Defaults

- When a tier is created for `opencode-zen` without explicit model choice, seed it with `big-pickle` (R9).
- For `openrouter`, wizard suggests the top free model by listing order as default entry.
- Every tier stores an ordered `entries` list; first entry is primary, rest are failover candidates **within the same tier** (R10).
