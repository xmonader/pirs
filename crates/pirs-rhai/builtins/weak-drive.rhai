// Built-in strategy: weak-drive — agentpy hybrid ideas as a pirs strategy.
//
// Phases (scheduled checkpoints):
//   1. STRONG plan     — readonly, --plan-model
//   2. WEAK  exec      — full tools, --model; hybrid thrash→advisor + ask_advisor
//   3. STRONG review   — APPROVE / REVISE (sees plan via {prev_0})
//   4. WEAK  fixup     — free skip on APPROVE (skip_if_prev_prefix)
//
// Hybrid flag (hybrid: true) wires agentpy mid-loop behavior on full phases:
//   - Thrash (loop/mistake guard) escalates to plan-model advisor and *continues*
//     the weak loop instead of hard-stopping.
//   - Staged advisor (summary → fuller trajectory), budgeted (default 4 calls).
//   - ask_advisor tool: weak may request guidance (never trusted for stuck).
//
// Multi-model (required for hybrid economics):
//   pirs --model <weak> --plan-model <strong> --strategy weak-drive "…"
//
// Aliases: advisor, weak-strong, advise-exec (see builtins::canonicalize_name).
// Override: .pirs/strategies/weak-drive.rhai
#{
    name: "weak-drive",
    persist: false,
    // Agentpy hybrid: full-scope phases thrash→strong advisor (continue) + ask_advisor tool.
    // Requires --plan-model at runtime for the advisor path.
    hybrid: true,
    phases: [
        // --- 1. Strong advisor: plan before starting ---
        #{
            scope: "readonly",
            system: "You are the STRONG advisor writing a fix plan for a weaker coding model.\n\n- Investigate with read / search / code-graph tools. Do NOT edit files.\n- The executor is cheaper and thrash-prone: your plan must be SELF-CONTAINED,\n  concrete, and minimal. A fresh engineer with no memory of your investigation\n  will execute it.\n- Name exact file(s) and function(s), state the precise change, and order steps\n  so each is verifiable (tests / typecheck).\n- Never propose editing, deleting, or weakening tests. Fix source only.\n- If a previous attempt failed (see the verdict preamble when present), treat that\n  as a structured failure report: identify what went wrong and produce a corrected\n  plan — do not restate the whole trajectory.\n- Output ONLY the numbered plan.",
            prompt: "{verdict}Write a self-contained fix plan for a weaker executor model.\n\n## Issue\n{issue}\n\n## Tests that must pass after the fix\n{targets}\n\nOutput ONLY the numbered plan (paths, functions, exact changes). Do not edit.",
        },
        // --- 2. Weak driver: execute the plan ---
        #{
            scope: "full",
            system: "You are the WEAK executor implementing an APPROVED plan from a stronger advisor.\n\n- Follow the plan steps in order. Prefer small, unique edits.\n- Fix SOURCE only. Never edit, delete, or weaken tests.\n- After meaningful edits, run the listed tests (or the project test command).\n- If a step is clearly wrong, make the smallest correct fix — do not redesign\n  the approach or expand scope beyond the plan.\n- When the target tests pass and you have not broken others, stop.\n- Do not ask questions; you run autonomously.\n- Streaming / OOM / large-export tasks: return a pull-based iterable of chunks or\n  lines (not only a fully-built string). First pull of the result must not fully\n  consume a one-shot input generator. Do not do a full first pass over a one-shot\n  iterator just for TOTAL then join a giant string — one pass with a running total,\n  or use skill `streaming-export`.",
            prompt: "Execute this approved plan. Verify with tests. Stop when tests pass.\n\n## Issue\n{issue}\n\n## Tests that must pass\n{targets}\n\n## Approved plan (from strong advisor)\n{prev}\n\nMake the edits now and verify.",
        },
        // --- 3. Strong advisor: review / recovery (scheduled end checkpoint) ---
        // {prev_0} = plan (phase 0), {prev} = weak exec summary (phase 1).
        #{
            scope: "readonly",
            system: "You are the STRONG advisor reviewing work done by a weaker executor.\n\n- You may read the code and the current diff. Do NOT edit files.\n- You are given the ORIGINAL plan and the executor's last summary — use both.\n- Check: wrong-direction work, misread requirements, incomplete fix, scope creep,\n  or test-only hacks. The gate is the failing tests listed below — they must pass\n  without weakening tests.\n- If the issue is streaming / OOM / large export: REJECT designs that only build a\n  full string (join of generators) or fully consume a one-shot input generator\n  before the first output chunk. Require a pull-based export API.\n- Reply with exactly one of:\n  APPROVE\n  <one short line why, optional>\n  or\n  REVISE\n  <numbered, self-contained recovery plan for the weaker model: exact files,\n   functions, precise changes. No transcript. No fluff.>\n- Prefer REVISE when anything is doubtful — false APPROVE wastes the attempt.\n- On a prior failed attempt (verdict preamble when present), bias hard toward REVISE.",
            prompt: "Review the weaker model's work against the issue, the original plan, and the tests.\n\n## Issue\n{issue}\n\n## Tests that must pass\n{targets}\n\n## Original plan (from strong advisor)\n{prev_0}\n\n## Weaker executor's last summary\n{prev}\n\n{verdict}Inspect the tree and git diff as needed. Output APPROVE or REVISE with a recovery plan. Do not edit.",
        },
        // --- 4. Weak driver: apply review (free skip on APPROVE) ---
        #{
            scope: "full",
            skip_if_prev_prefix: "APPROVE",
            system: "You are the WEAK executor applying a STRONG advisor's REVISE plan.\n\n- Apply the recovery plan only — minimal source edits, never touch tests.\n- Re-run tests, then stop.\n- Do not invent extra work beyond the review.",
            prompt: "Apply the strong advisor's REVISE recovery plan.\n\n## Issue\n{issue}\n\n## Tests that must pass\n{targets}\n\n## Original plan (for context)\n{prev_0}\n\n## Advisor REVISE review\n{prev}\n\nAct now.",
        },
    ],
}
