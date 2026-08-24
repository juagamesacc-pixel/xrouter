# 02 — Configuration & Interactive Wizard

## Storage

`~/.config/xrouter/config.toml` (permissions `0600`; keys are secrets):

```toml
[settings]
default_tier = "fast"

[providers.opencode-zen]
kind = "openai-compat"
base_url = "https://opencode.ai/zen/v1"        # confirm exact URL at build time
enabled = true
keys = ["sk-...", "sk-..."]                      # multiple keys → round-robin
rr_cursor = 0                                    # persisted cursor (optional continuity)

[providers.openrouter]
kind = "openai-compat"
base_url = "https://openrouter.ai/api/v1"
enabled = true
keys = ["sk-or-..."]

[providers.anthropic]                            # optional user-added
kind = "anthropic"
base_url = "https://api.anthropic.com"
keys = ["sk-ant-..."]

# A tier = named alias over one or more concrete models.
# Requesting tier name routes STRICTLY to these entries (see 05).
[[tiers]]
name = "big-pickle"                              # custom model name / alias
entries = [
  { provider = "opencode-zen", model = "big-pickle", is_default = true },
]

[[tiers]]
name = "free-auto"
entries = [
  { provider = "opencode-zen", model = "mimo-v2.5-free" },
  { provider = "openrouter",   model = "deepseek/deepseek-r1:free" },   # alt provider, same tier
]
```

## Wizard flow (`xrouter wizard`)

Interactive TUI using `dialoguer` (plain prompts, no heavy TUI framework):

```
1. Provider setup
   ? Select provider to configure
     ❯ opencode-zen      (built-in default)
       openrouter        (built-in default)
       anthropic         (optional)
       custom openai-compatible…
       done — finish wizard

2. For chosen provider:
   ? Paste API key (empty line = stop adding)
     → key 1 saved
     → key 2 saved
     → (enter) done
   ✔ 3 keys stored for opencode-zen — round-robin enabled per request.

3. Model discovery (R7):
   Fetching models from opencode-zen… ✔ 42 models
   Showing FREE models only:
     [ ] mimo-v2.5-free
     [ ] qwen3-coder-[free]
     [ ] some-model(free)
   ? Space to select, Enter to confirm. Option: [a] show all models instead,
     [c] add custom model name manually.

4. Custom alias (R8): after selecting models:
   ? Create a tier/alias name for the selection (e.g. "fast", "smart")
     > big-pickle
   ? Default model within this tier when none specified: big-pickle (for opencode-zen)

5. Failover prompt:
   ? If opencode-zen fails, try another provider in this tier? [Y/n]
     → if yes: repeat step 3 for another provider and append to same tier.

6. Loop back to provider menu until "done".
```

Wizard rules:
- Re-running `wizard` edits existing config non-destructively (keys append/remove via sub-menu).
- Keys never echoed back; masked as `sk-…last4`.
- Validation: on save, fire one cheap authenticated request (`GET /models`) per key; mark invalid keys with a warning but allow saving.
- Config writes are atomic (temp file + rename).

## CLI surface

| Command | Purpose |
|---|---|
| `xrouter wizard` | interactive setup |
| `xrouter serve [--port N]` | start router |
| `xrouter keys add <provider>` | append key(s) |
| `xrouter models list [provider] [--free]` | list cached/live models |
| `xrouter tier add <name> --provider P --model M [--more …]` | non-interactive tier creation |
| `xrouter test <tier>` | send probe request through full routing path |
