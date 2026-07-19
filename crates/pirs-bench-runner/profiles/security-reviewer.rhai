// A profile is a *role* — a strategy bundled with the persona, model, and tool
// policy that make it a virtual employee. This one is a security-minded fixer:
// it runs the plan → critic → execute loop, but every phase is stamped with a
// paranoid-reviewer persona, the critic runs on a stronger model (via the
// strategy's own Oracle lever if you swap to plan-oracle-exec), and the shell is
// denied so the role can only read, edit, and run the suite.
//
//   pirs-bench solve --provider deepseek --model deepseek-v4-flash \
//       --profile this-file.rhai --issue "..." --targets "tests/t.py::test_x"
//
// `resolved_strategy()` prepends the persona to each phase's system prompt and
// fills the model into any phase that didn't pin its own, so the whole run
// inherits the role without new machinery.

#{
    name: "security-reviewer",
    persona: "You are a paranoid senior security engineer. Treat every input as \
              hostile, prefer the smallest safe change, and never weaken a check to \
              make a test pass.",
    model: "deepseek-v4-flash",
    strategy: "plan-critic-exec",
    tools: #{
        // No raw shell for this role — it reads, edits, and runs the suite via the
        // run_tests compound tool, nothing more.
        deny: ["bash"],
    },
}
