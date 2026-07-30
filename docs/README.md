# Documentation index

Start here if you are new. Coding agents should also read the root
**[AGENTS.md](../AGENTS.md)** (injected into the system prompt when present).

## Onboarding

| Doc | Audience | Contents |
|-----|----------|----------|
| [../AGENTS.md](../AGENTS.md) | Agents (+ humans) | Map, build, conventions, weak-drive, do/don't |
| [DEVELOPMENT.md](DEVELOPMENT.md) | Humans | Workspace layout, test matrix, where to edit |
| [../README.md](../README.md) | Everyone | Product quickstart, CLI, extensions, skills |
| [PRODUCTS.md](PRODUCTS.md) | Everyone | Portfolio: pirs vs pirs-claw vs power tools |
| [ROADMAP.md](ROADMAP.md) | Everyone | North star, do-not list, capability matrix |

## Core topics

| Doc | Contents |
|-----|----------|
| [STRATEGIES.md](STRATEGIES.md) | Builtin strategies, weak-drive, hybrid thrash→advisor |
| [MODELS.md](MODELS.md) | Pin vs portable models, backends, keys |
| [hybrid-model-economics.md](hybrid-model-economics.md) | Measured strong-plan / weak-exec matrices |
| [SWE-QA.md](SWE-QA.md) | SWE-bench Lite campaigns, leaderboard, how to land results |
| [WORK-CONTEXT.md](WORK-CONTEXT.md) | Multi-root `--cwd` / `--also` / contexts |
| [TUI-JOURNEY.md](TUI-JOURNEY.md) | First-run TUI walkthrough |
| [../extensions/README.md](../extensions/README.md) | Packs, profiles, load order |

## Products & ops

| Doc | Contents |
|-----|----------|
| [pirs-claw.md](pirs-claw.md) | Personal agent / gateway |
| [speech.md](speech.md) | Speech setup |
| [telegram-checklist.md](telegram-checklist.md) | Telegram ops |
| [mcp-email-calendar.md](mcp-email-calendar.md) | MCP mail/calendar |
| [mcp-scale.md](mcp-scale.md) | MCP scale notes |

## Strategy / comparison / transfer

| Doc | Contents |
|-----|----------|
| [PLAN-FORWARD.md](PLAN-FORWARD.md) | Near-term engineering plan |
| [VIABILITY-VS-HERMES-OPENCLAW.md](VIABILITY-VS-HERMES-OPENCLAW.md) | Peer comparison |
| [HERMES-GAPS.md](HERMES-GAPS.md) | Gap list vs Hermes-class ops |
| [shrimp-transfer.md](shrimp-transfer.md) | Transfer notes |
| [../PIRS-BENCH-PLAN.md](../PIRS-BENCH-PLAN.md) | Bench design plan (historical + design) |
| [../crates/pirs-bench/docs/SWE-BENCH-LITE.md](../crates/pirs-bench/docs/SWE-BENCH-LITE.md) | Lite instance prep runbook |

## QA artifacts (not prose docs)

| Path | Contents |
|------|----------|
| [../qa/README.md](../qa/README.md) | Live QA log index |
| [../qa/bench-swebench-5x5/LEADERBOARD.md](../qa/bench-swebench-5x5/LEADERBOARD.md) | Strict Lite-50 scores |
| `../qa/bench-swebench-5x5/results_*/REPORT*.md` | Per-campaign reports |

## When you change behavior

1. Code + tests in the owning crate (see [DEVELOPMENT.md](DEVELOPMENT.md)).
2. If user-facing: update root README and/or the topic doc above.
3. If strategy/hybrid: update [STRATEGIES.md](STRATEGIES.md) and [AGENTS.md](../AGENTS.md).
4. If SWE campaign: land `results_*` + update leaderboard per [SWE-QA.md](SWE-QA.md).
