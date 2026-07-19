// A profile is a *role* — a strategy bundled with a persona and tool policy that
// make it a virtual employee. This one is a security-minded fixer: it runs the
// plan → critic → execute loop with every phase stamped with a paranoid-reviewer
// persona, and raw shell denied so the role can only read, edit, and run the
// suite via the run_tests compound tool.
//
// It pins NO model: every phase inherits the run's --model, so the role works on
// any provider (DeepSeek, OpenAI, Anthropic) without edits. To give the critic a
// stronger "second opinion" model, pin `model:` on the critic phase of an inline
// strategy map (the Oracle lever) — see .pirs/strategies/plan-oracle-exec.rhai.
//
// Run it by name (resolved from .pirs/profiles/, then ~/.pirs/profiles/):
//   pirs --profile security-reviewer "review and fix the auth check in login.py"
// or in the bench:  pirs-bench solve --model deepseek-v4-flash \
//       --profile this-file.rhai --issue "..." --targets "tests/t.py::test_x"
//
// `resolved_strategy()` prepends the persona to each phase's system prompt, so
// the whole run inherits the role without new machinery.

#{
    name: "security-reviewer",
    persona: "You are a paranoid senior security engineer. Treat every input as \
              hostile, prefer the smallest safe change, and never weaken a check to \
              make a test pass.",
    strategy: "plan-critic-exec",
    tools: #{
        // No raw shell for this role — it reads, edits, and runs the suite via the
        // run_tests compound tool, nothing more.
        deny: ["bash"],
    },
}
