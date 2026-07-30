# Hero Shrimp → pirs transfer map

Survey of `~/hero/code/hero_shrimp` for ideas worth reusing in pirs vs
explicitly **skipping** (bloat or already covered). Goal: lean dual products
**pirs** (harness) and **pirs-claw** (agent: code + chat + schedule + gateway),
not a shrimp fork.

| Capability | Verdict | Rationale |
|------------|---------|-----------|
| **Independent review / done-gates** (stronger model reviews every diff; vacuous-green refuse) | **Useful** | Core shrimp insight: cheap executor + blocking reviewer. pirs already has `review-gate.rhai`, `verify-guard.rhai`, stop-gate in weak pack — **prefer packs + `--plan-model`**, not a second review engine. |
| **Durable goals / runs** (goal dir + resume after restart) | **Useful (light)** | pirs has `goal.rhai`, `runs.rhai`, sessions JSONL. Claw/work should **reuse session files**, not shrimp’s goals/ RPC surface. |
| **Schedules / cron** | **Useful (one path)** | Hermes/OpenClaw/shrimp all treat cron as load-bearing for “always-on”. **pirs-claw** gets a **minimal schedule store + tick**, not Hero service lifecycle. |
| **Multi-provider routing** | **Useful — already in pirs** | shrimp.yml backends ≈ `~/.pirs/config.toml` `[[backends]]`/`[[models]]` + `RoutingProvider`. **Do not reimplement.** |
| **Channels** (Telegram, WhatsApp, …) | **Hermes set shipped** | telegram/discord/slack/whatsapp/signal + pairing; not full OpenClaw matrix. |
| **Wire-log / crypto audit** (Ed25519 hash-chain) | **Skip (defer)** | Strong for compliance; heavy and shrimp-specific. pirs has audit-log pack + `--trace` JSONL without crypto. |
| **Web UI** (Dioxus operator UI) | **Skip** | Full stack; pirs already has REPL/TUI/`--serve`. No second UI framework. |
| **OpenRPC / Hero service manager** | **Skip** | Ops surface for Hero fleet, not needed for standalone pirs/claw. |
| **OTEL / full observability stack** | **Skip** | pirs has `--trace` + session stats; full OTEL is shrimp_otel bloat for now. |
| **USD spend caps (persistent)** | **Useful idea — pack exists** | `spend-caps.rhai` already; wire into claw defaults if needed, don’t port meter code. |
| **Skill evolution / RL trajectories** | **Skip** | Research/eval surface; out of product scope. |
| **Council / multi-agent debate** | **Skip for default** | pirs has arena/relay packs; not default for work/claw. |

## OpenClaw / Hermes (shape only)

| Idea | For pirs-claw? |
|------|----------------|
| Always-on gateway + many channels | **Partial** — Hermes set (5 channels), not OpenClaw zoo |
| Session memory across restarts | **Yes** — JSONL + FTS memory |
| Cron / scheduled prompts | **Yes** — lean store + `tick` + deliver targets |
| Skill self-improvement loop | **Partial** — skills dir + crystallizer pack |
| Desktop hub / mobile | **No** |

## Product split

| Product | Role | Peers |
|---------|------|--------|
| **pirs** | Full harness / TUI / strategies / power tools | pi, qwen-code core |
| **pirs-claw** | Daily agent: code + chat + schedule + gateway | Claude Code / Codex CLI; Hermes / OpenClaw (thinner) |

Neither product vendors shrimp source; both depend on `pirs-agent` / `pirs-tools` / `pirs-ai` / registry.
See [PRODUCTS.md](PRODUCTS.md).

## Evidence-driven follow-ups (2026-07-30)

From shrimp **run reports** + external code review (not taken as design gospel):

| pirs change | Rationale |
|-------------|-----------|
| Hard `shell_safety` denials (newline-safe) | Avoid catastrophic patterns even if packs are off |
| Edit oscillation in `ThrashGuard` (A→B→A) | Harness-owned stuck signal (hybrid can escalate) |
| `weak-model` only clears stop-gate on real verify | `ls`/`echo` must not count as tests |
| Completeness nudge after first successful edit | Multi-site incomplete fixes dominate clean misses |
| `scope-creep` pack | Env/build file thrash (setup/tox/requirements) |

Re-validate with pirs Lite/smoke before claiming score impact.
