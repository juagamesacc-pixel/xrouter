# xrouter — Project Overview

**xrouter** is a blazing-fast, highly resilient LLM request router written in Rust. It sits between clients (Claude Code, OpenAI SDKs, curl, any agent) and upstream LLM providers, exposing **both** API surfaces:

- `POST /v1/messages` — Anthropic-compatible
- `POST /v1/chat/completions` — OpenAI-compatible

Any client can hit either endpoint; xrouter normalizes the payload and forwards to whichever provider/model serves the resolved tier.

## Core Requirements (source of truth)

| # | Requirement |
|---|-------------|
| R1 | Accept Anthropic **and** OpenAI-compatible requests simultaneously |
| R2 | Fastest possible routing path (hot path with zero unnecessary allocations/serialization) |
| R3 | Interactive setup wizard: save providers + multiple API keys per provider |
| R4 | Round-robin key rotation per provider — every subsequent request uses the next key |
| R5 | Highly resilient: retries, timeouts, circuit breakers, failover |
| R6 | Default providers: **opencode-zen** and **openrouter** |
| R7 | Model discovery filters **free models** by name tag (`[free]`, `(free)`, or suffix `-free`, e.g. `mimo-v2.5-free`) |
| R8 | User may attach a **custom model name** (alias) to any model — the alias names a **tier** |
| R9 | `big-pickle` is the default model for opencode-zen (and an equivalent default for openrouter) |
| R10 | Automatic failover to alternative providers **within the same tier only** |
| R11 | Strict tier semantics: a request naming a tier never degrades or upgrades to another tier; if all models/providers on that tier are exhausted → return error |

## Document Map

1. `01-architecture.md` — crates, modules, data flow
2. `02-config-and-wizard.md` — storage format + interactive wizard flow
3. `03-providers.md` — provider adapters, defaults, free-model discovery
4. `04-load-balancing.md` — round-robin keys, resilience machinery
5. `05-tiers.md` — tier model and strict routing rules
6. `06-api-surface.md` — HTTP endpoints & protocol translation
7. `07-performance.md` — latency budget and fast-path engineering
8. `08-resilience.md` — failure handling detail
9. `09-roadmap.md` — build milestones
