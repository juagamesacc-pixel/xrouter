# 09 — Roadmap & Milestones

## M1 — Skeleton (foundation)
- [ ] Cargo workspace with the five crates from 01.
- [ ] Core types: `Tier`, `ModelEntry`, `ProviderId`, error taxonomy.
- [ ] Config load/save (atomic writes, 0600 perms), hot-reload watcher.

## M2 — Wizard & defaults
- [ ] `xrouter wizard` full flow (02): provider menu → multi-key intake → model fetch → free filter → custom alias → failover opt-in.
- [ ] Built-in `opencode-zen` + `openrouter` providers; `big-pickle` default seeding for opencode-zen (R6, R9).
- [ ] `keys add`, `models list [--free]`, `tier add` CLI commands.

## M3 — Routing engine
- [ ] KeyRing round-robin (R4), health state machine, DashMap endpoint registry.
- [ ] Strict tier resolution (05) incl. `tier_exhausted` semantics (R11).

## M4 — Server & protocols
- [ ] Axum server: `/v1/chat/completions`, `/v1/messages`, `/v1/models`.
- [ ] OpenAI↔Anthropic translation (request + streaming SSE state machine).
- [ ] Failover retry loop within tier (R10); error mapping table (06).

## M5 — Performance & resilience hardening
- [ ] Two-phase parse fast path, byte passthrough when protocols match.
- [ ] Connection pools, rustls reuse, allocation pass.
- [ ] Circuit breakers tuned; chaos tests from 08 green.

## M6 — Polish
- [ ] Prometheus metrics + JSON logs.
- [ ] Benchmarks vs acceptance targets (07).
- [ ] README, example configs, `xrouter test <tier>` smoke command.

## Verification strategy
- Unit: free-model filter cases (`[free]`, `(free)`, `-free`, `:free` negatives), RR cursor wraparound, tier strictness.
- Integration: wiremock-based fake upstreams for both protocols; scripted failure sequences.
- E2E: real opencode-zen/openrouter smoke behind `--allow-live` flag.

## Risks / open questions
- Exact opencode-zen base URL & model-list schema — verify against live API during M2.
- OpenRouter free-model naming uses `:free` suffix — confirm no other free markers needed.
- Anthropic tool-call ↔ OpenAI tool-call edge cases (parallel tool use) — cover in M4 tests.
