// weak-model.rhai — loop hardening for weaker models.
// Install: cp to .pirs/extensions/ (project) or ~/.pirs/extensions/ (global),
// or enable with `pirs --weak` (loads the bundled copy automatically).
//
// Behaviors:
// 1) Loop detection — block the same tool call repeated MAX_REPEAT times
// 2) Path-level edit thrash — after MAX_EDIT_FAILS failures on one path, block + steer
// 3) No-progress — many tool rounds without a successful mutate or verify → steer
// 4) Verify-after-edit — force build/test after file mutations
// 5) Stop gate — ending without verify since last edit injects follow-up (fire-capped)
// 6) Plan pinning — update_plan keeps a visible plan at the context tail

const PLAN_PIN = "[current-plan pinned by extension]";
const MAX_REPEAT = 3;
const MAX_EDIT_FAILS = 2;
const MAX_NO_PROGRESS = 6;
const MAX_STOP_FIRES = 2;

fn is_mutate(name) {
    name == "edit" || name == "edit_block" || name == "write" || name == "safe_edit" || name == "ast_edit"
}

fn on_tool_call(id, name, args) {
    let key = name + "|" + args.to_json();
    let counts = if state_has("call_counts") { state_get("call_counts") } else { #{} };
    let n = (counts[key] ?? 0) + 1;
    counts[key] = n;
    state_set("call_counts", counts);

    if n >= MAX_REPEAT {
        state_set("nudge", true);
        state_set("nudge_kind", "repeat");
        return #{
            block: true,
            reason: `This exact call was already made ${n} times and keeps failing or changing nothing. Stop: inspect the last error, change strategy, or ask the user.`
        };
    }

    if is_mutate(name) && args.contains("path") {
        let path = `${args.path}`;
        state_set("last_mutate_path", path);
        let fails = if state_has("edit_fails") { state_get("edit_fails") } else { #{} };
        let f = fails[path] ?? 0;
        if f >= MAX_EDIT_FAILS {
            state_set("nudge", true);
            state_set("nudge_kind", "edit_fail");
            state_set("edit_fail_path", path);
            return #{
                block: true,
                reason: `edit/write on '${path}' already failed ${f} times. Re-read the full function, use a larger unique oldText, or call safe_edit if available — do not retry the same failing patch.`
            };
        }
    }

    let idle = if state_has("idle_rounds") { state_get("idle_rounds") } else { 0 };
    state_set("idle_rounds", idle + 1);
    if idle + 1 >= MAX_NO_PROGRESS {
        state_set("nudge", true);
        state_set("nudge_kind", "no_progress");
    }
    ()
}

fn on_tool_result(id, name, result) {
    let err = result.isError;

    if is_mutate(name) {
        if !err {
            state_set("needs_verify", true);
            state_set("had_successful_mutate", true);
            state_set("verified_since_edit", false);
            state_set("idle_rounds", 0);
            if state_has("last_mutate_path") {
                let path = state_get("last_mutate_path");
                let fails = if state_has("edit_fails") { state_get("edit_fails") } else { #{} };
                fails[path] = 0;
                state_set("edit_fails", fails);
            }
        } else if state_has("last_mutate_path") {
            let path = state_get("last_mutate_path");
            let fails = if state_has("edit_fails") { state_get("edit_fails") } else { #{} };
            let f = (fails[path] ?? 0) + 1;
            fails[path] = f;
            state_set("edit_fails", fails);
            if f >= MAX_EDIT_FAILS {
                state_set("nudge", true);
                state_set("nudge_kind", "edit_fail");
                state_set("edit_fail_path", path);
            }
        }
    }

    // Successful bash after an edit counts as verification / progress.
    if name == "bash" && !err {
        state_set("idle_rounds", 0);
        if state_has("had_successful_mutate") && state_get("had_successful_mutate") {
            state_set("needs_verify", false);
            state_set("verified_since_edit", true);
            state_set("had_test_run", true);
        }
    }

    ()
}

fn on_steering() {
    if state_has("nudge") && state_get("nudge") {
        state_set("nudge", false);
        let kind = if state_has("nudge_kind") { state_get("nudge_kind") } else { "repeat" };
        if kind == "edit_fail" {
            let path = if state_has("edit_fail_path") { state_get("edit_fail_path") } else { "the file" };
            return `Your edits to ${path} keep failing. Re-read the full surrounding function with read, then either (a) use edit_block with a SEARCH/REPLACE of a larger unique block, (b) retry edit with more surrounding context, or (c) call safe_edit with a clear instruction. Do not retry the same oldText.`;
        }
        if kind == "no_progress" {
            state_set("idle_rounds", 0);
            return "You have made several tool calls without a successful file change or test run. Stop thrashing: restate the goal in one sentence, list what you know, then take ONE different concrete step (read a specific file or run tests).";
        }
        return "You are repeating the same action. Re-read the error output, list what you know, and try a DIFFERENT approach.";
    }
    if state_has("needs_verify") && state_get("needs_verify") {
        state_set("needs_verify", false);
        return "You just modified files. Before continuing: run the project's build and tests with bash, then fix any failures. Do not claim the task is done until they pass.";
    }
    ()
}

// Fire-capped stop gate: force another turn if the model ends after edits without verify.
fn on_follow_up() {
    let fires = if state_has("stop_fires") { state_get("stop_fires") } else { 0 };
    if fires >= MAX_STOP_FIRES {
        return ();
    }
    if state_has("had_successful_mutate") && state_get("had_successful_mutate") {
        let verified = state_has("verified_since_edit") && state_get("verified_since_edit");
        if !verified {
            state_set("stop_fires", fires + 1);
            state_set("needs_verify", true);
            return [
                "STOP GATE: you edited files but have not shown a successful test/build run since. Run the project tests with bash now. Only finish after they pass (or report the real failures)."
            ];
        }
    }
    ()
}

register_tool("update_plan", "Set or update the working plan (markdown checklist). Call at the start of a task and whenever steps complete or change.", #{
    type: "object",
    properties: #{ plan: #{ type: "string" } },
    required: ["plan"]
});

fn tool_update_plan(args) {
    state_set("plan", args.plan);
    "Plan updated. It will stay pinned at the tail of the context."
}

fn on_context(messages) {
    if !state_has("plan") {
        return messages;
    }
    let plan = state_get("plan");
    let filtered = messages.filter(|m|
        !(m.role == "user" && m.content.to_string().starts_with(PLAN_PIN))
    );
    let pin = #{
        role: "user",
        content: PLAN_PIN + "\n" + plan + "\n[keep this plan updated with the update_plan tool as you progress]",
        timestamp: 0
    };
    let idx = if filtered.len() > 0 { filtered.len() - 1 } else { 0 };
    filtered.insert(idx, pin);
    filtered
}
