# xrouter Orchestration Rules — How Subagents Were Managed

This document records exactly how the **orchestrator** managed this project end-to-end: todo preparation, lane planning, subagent delegation, background-task discipline, and verification. It is the rulebook for reproducing the same workflow.

---

## 1. Orchestrator Role

- **Orchestrator is not the default implementation worker.** It plans, schedules, delegates, monitors, reconciles, and verifies specialist work.
- **Direct execution is allowed only when:** one isolated, clear, low-risk action where delegation overhead exceeds execution (e.g., writing this README2.md, a single 20-line edit). Multi-step implementation, broad discovery, external research, or complex debugging is always delegated.
- **Optimize for** quality, speed, cost, reliability by dispatching the right specialist lanes and integrating terminal results into one coherent outcome.

## 2. Todo Preparation (Single Source of Truth)

Created at project start via `todowrite` and maintained continuously:

```text
1. Verify Rust toolchain (user confirms rustc 1.98.0 / cargo 1.98.0 / rustup 1.29.0 ready - no install needed) — high
2. Coder: complete project structure & code (no toolchain work) — high
3. Auditor: review code vs plan (R1-R11) — high
4. Builder/fixer: build & fix errors (never divert from plan) — high
```

Rules followed:
- **One `in_progress` at a time.** Only the active lane is `in_progress`; others are `pending` or `completed`.
- **Continuity:** When user appends a new task while a list exists, append to end, preserve order/status/priority unless explicitly told to reprioritize.
- **Real-time updates:** Mark `completed` only after required work is actually done including verification, never on intent. If blocked, keep `in_progress` and add blocker todo.
- **Exactly one lane owns writes at a time** for overlapping paths (e.g., `xrouter-server/src/lib.rs`).

## 3. Lane Identification & Work Graph

Before dispatch, built a short work graph:

**Independent lanes (can run in parallel):**
- `@explorer` recon + `@librarian` research (not needed here; plan was already grounded)

**Dependency-ordered lanes (must be sequential):**
```
coder (M1-M6 structure/code) → auditor (oracle, read-only) → builder/fixer (build & fix)
       │
       └─► toolchain is pre-installed per user, so coder was explicitly forbidden to touch rustup/apt
```

**Advisory ownership:**
- `coder`: owns `xrouter/**` file creation/structure (except toolchain)
- `oracle`: read-only, owns audit report (no writes)
- `fixer`: owns bounded code fixes within same files, must not redesign

If two parts could proceed independently they were dispatched in parallel before dependent work. Here the pipeline is strictly sequential, so no parallel writer lanes were used.

## 4. Specialist Selection

| Specialist | When Used Here | Permissions | Stats |
|---|---|---|---|
| `@coder` (custom, `opencode/big-pickle` + fallback `muse-spark-1.2-contributor-free`) | Large, plan-driven implementation spanning 6 crates, dep pinning, staged coding, strict plan adherence | read_files, write_files | disciplined senior coder: meta-todo → dep research → deps-doc → mini subtasks → verify |
| `@oracle` | Audit vs R1-R11, architecture risk, review | read_files only | 5x better decision, review gate |
| `@fixer` | Bounded build fixes, streaming passthrough, admin-models, dead-code removal | read_files, write_files | 2x faster edits, 1/2 cost |
| `@librarian` / `@explorer` | Not dispatched (plan + live API already grounded, no unfamiliar lib) | — | — |

Rule of thumb enforced: *Headless/mechanical implementation → @fixer. Full-project plan execution → @coder. Review → @oracle.*

## 5. Dispatch Efficiency

- **Brief delegation notices** to user before each call (“Dispatching @coder…”) not verbose essays.
- **Reference paths/lines, don’t paste files** (`crates/xrouter-server/src/lib.rs:49` not full contents).
- **Record task IDs, state, ownership/dependency labels** and track on Background Job Board.
- **Do not immediately wait** after spawning independent background tasks unless next step truly depends on result — stay unblocked, reconcile when notified via hook.

## 6. Background Task Discipline (Strict)

1. **Before dispatch**, check Background Job Board for existing task covering same objective.
2. **Never fetch output via `task(..., task_id)`** — that resumes the child and starts new work. Use:
   - `task_result(task_id)` — returns completed final message only (callable by any owner)
   - `task_status(task_id)` — read-only live inspection
   - `task_message(task_id, msg)` — queue concise, non-interrupting message (does not launch/resume)
3. **Cancel only when obsolete/wrong/conflicting:** `task_cancel(task_id)` retains session for `task_revive`; inspect partial changes before replacement.
4. **Revive via `task_revive(task_id, prompt)`** for cancel-and-resume in same session.
5. **Prefer `background:true`** for delegated work that can run independently.
6. **End turn immediately after spawning** independent background tasks with brief status; do not poll; wake scheduler resumes on completion.

Applied here:
- `cod-1` (ses_fcdf…) stalled 13m mid-stream (dead SSE, no TCP, no files) → `task_revive` aborted but landed `error (unconfirmed)`; clean respawn as `cod-2`.
- `cod-2` (ses_fcde…) hit `AI_APICallError: Unable to connect` on `big-pickle` with empty result → respawn as `cod-3` (code-only, no toolchain) which succeeded.
- `ora-1` returned empty → immediately rerouted as `ora-2` with tighter prompt, succeeded.
- `fix-1` returned empty → rerouted as `fix-2` (bounded 5-fix scope) and reconciled.

## 7. Session Reuse

- Reusable sessions listed under **Reusable Sessions** may be resumed by alias (e.g., `cod-3`, `ora-2`) via `task(..., task_id:"cod-3")`. Active/Unreconciled are *not* resumable.
- Prefer reuse over creation: `cod-3` reused Cargo.lock + deps-doc context (2000 lines + 460+339+231… lines) to avoid re-reading.
- Always pass `task_id` explicitly when reusing; omitting it creates a new session.

This run reused:
- `cod-3` context for auditor/fixer (they cited its reads: `xrouter-server/lib.rs 460→580 lines` etc.)
- `ora-2` + `fix-2` both marked `completed, reconciled` and remain reusable for follow-ups.

## 8. Delegation Contract & Grounding Rules

Every delegation named a **validation owner** and **allowed scope**, with strict grounding:

- **Coder prompt:** “NO GUESSING. Every API usage must come from official docs at pinned version or verified facts (opencode-zen `https://opencode.ai/zen/v1`, openrouter `https://openrouter.ai/api/v1`, live free-model list, `big-pickle` default). One self-fix → websearch exact error → documented fix. If plan conflicts reality, STOP and flag.”
- **Auditor prompt:** read-only, quote `plan/*.md` + file:line refs, PASS/FAIL per R1-R11, websearch if uncertain.
- **Fixer prompt:** bounded 5-fix priority list, `cargo check/test` where possible, never divert from plan, preserve quality (no `unwrap` in library paths, handle errors).

Credentials handling: `openrouter`/`opencode` keys written only to `~/.config/xrouter/config.toml` (0600) or env vars, never in source or git; GitHub PAT `ghp_9im...` configured via `git credential approve` + `chmod 600`.

## 9. Coder Internal Discipline (Meta-Todo + Context Control)

The custom `coder` agent itself follows:

1. **Plan intake** — read all `plan/*.md`.
2. **Meta todo** — `todowrite` covering dep research → deps-doc → M1-M6 subtasks → verification.
3. **Dep research** — pick minimal stable deps, pin exact compatible combo (e.g., `tokio 1.47.1`, `axum 0.7.9`, `hyper 1.7.0`, `reqwest 0.12.23 rustls-webpki`).
4. **Docs before code** — `websearch`/`webfetch` per pinned dep version → multi-file `deps-doc/` (one file per dep: correct API, gotchas, examples, anti-patterns).
5. **Staged coding** — mini independent subtasks per crate/module, one `in_progress` at a time, verify each compiles.
6. **Error protocol** — one self-fix, then websearch, log pitfall into deps-doc.
7. **Context discipline** — after subtask, write 3-5 line handoff, drop code from context; re-read only via `grep`+offset, reference `path:line`.

This prevents token bloat on large codebases — what was enforced here (coder left `pick_weighted` intact rather than re-reading whole file).

## 10. Design Handoff & Verification

- `@designer` not used (no UI). If used, its layout/spacing/motion is intentional and not to be simplified.
- Verification reused evidence; not repeated unless final state changed. Auditor could not run builds (linker `-lgcc_s` / glibc vs bionic mismatch on Termux — environment, not code), so fixer validated manually and noted `cargo verify-project` OK.
- Final synthesis reconciles all writer lanes, resolves conflicts, and gates dependent lanes (no next lane starts until prior lane’s terminal result is reconciled).

## 11. Failure Handling in This Run

| Issue | Detection | Action |
|---|---|---|
| `cod-1` dead stream 13m, no TCP, 0 files | `task_status` `possibly_stuck:true` + `ss -tnp` + `find` | `task_revive` → error state → clean respawn `cod-2` |
| `cod-2` `AI_APICallError` empty result | `task_result` no completed text | respawn `cod-3` code-only (no toolchain) — succeeded, 6 crates + README |
| `ora-1` empty | `task_result` empty | reroute `ora-2` tight prompt — full R1-R11 report |
| `fix-1` empty | `task_result` empty | reroute `fix-2` bounded 5-fix scope — streaming passthrough + model-cache + max_attempts fixed, dead `weighted.rs` removed |
| Linker `-lgcc_s` / glibc vs bionic | `cargo check` error | Not retried as toolchain install; documented as host mismatch, code manually validated |

## 12. How to Reproduce

1. Ensure toolchain `rustc 1.98.0` / `cargo 1.98.0` on a **glibc host** (Ubuntu/Debian/Docker, not Termux bionic).
2. Create todos (section 2) before any dispatch.
3. Dispatch `coder` with code-only scope + grounding rules (section 8).
4. On `coder` terminal, dispatch `oracle` audit (read-only).
5. After reconciling audit, dispatch `fixer` with bounded priority fix list.
6. Use `task_result`/`task_status`/`task_message` correctly (section 6), reuse sessions by alias, end turn after spawns, and verify before closing.

---

*Generated by orchestrator on 2026-08-24. Reusable sessions: cod-3, ora-2, fix-2 remain available for incremental follow-ups.*
