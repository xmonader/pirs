// weak-model.rhai — loop hardening for weaker models.
// Install: cp to .pirs/extensions/ or enable with `pirs --weak`.
//
// Behaviors:
// 1) Loop detection — path + normalized-args fingerprint (not only exact JSON)
// 2) Path-level edit thrash — after MAX_EDIT_FAILS failures, block + steer
// 3) No-progress — many rounds without mutate/verify → steer
// 4) Verify-after-edit — force build/test after file mutations
// 5) Stop gate — ending without verify injects <system-reminder> (fire-capped)
// 6) Plan pin via update_plan using the same <system-reminder> channel

// Note: rhai top-level const is NOT visible inside nested script functions
// (only the host-called entry). Use string literals inside helpers.
const MAX_REPEAT = 3;
const MAX_EDIT_FAILS = 2;
const MAX_NO_PROGRESS = 6;
const MAX_STOP_FIRES = 2;

fn is_mutate(name) {
    name == "edit" || name == "edit_block" || name == "write" || name == "safe_edit" || name == "ast_edit"
}

// Normalize thrash key: tool name + path (if any) + arg JSON with whitespace
// collapsed. Exact JSON identity is too brittle for weak models that reformat.
fn thrash_key(name, args) {
    let path = if args.contains("path") {
        `${args.path}`
    } else if args.contains("target_file") {
        `${args.target_file}`
    } else if args.contains("file_path") {
        `${args.file_path}`
    } else {
        ""
    };
    // Path + tool name is the stable thrash signal; full JSON is secondary.
    // Include a short args fingerprint so different commands on same path count separately.
    let raw = args.to_json();
    let parts = raw.split(" ");
    let cleaned = str_join(parts, " ");
    name + "|" + path + "|" + cleaned
}

fn wrap_reminder(kind, body) {
    // Literals (not top-level const) so nested fns can see them.
    "<system-reminder> kind=" + kind + "\n" + body + "\n</system-reminder>"
}

// Plan pins only — control reminders (stop_gate, verify, edit_fail, …) must
// stay in context after update_plan. Matching any <system-reminder> would
// strip those and leave only the re-appended plan pin.
fn is_plan_pin(m) {
    if m.role != "user" {
        return false;
    }
    let t = m.content.to_string();
    t.contains("<system-reminder>") && t.contains("kind=plan")
}

fn on_tool_call(id, name, args) {
    let key = thrash_key(name, args);
    let counts = if state_has("call_counts") { state_get("call_counts") } else { #{} };
    let n = (counts[key] ?? 0) + 1;
    counts[key] = n;
    state_set("call_counts", counts);

    if n >= MAX_REPEAT {
        state_set("nudge", true);
        state_set("nudge_kind", "repeat");
        return #{
            block: true,
            reason: `This call fingerprint was already used ${n} times (tool=${name}). Stop: inspect the last error, change strategy, or ask the user.`
        };
    }

    if is_mutate(name) {
        let path = if args.contains("path") { `${args.path}` } else { "" };
        if path != "" {
            state_set("last_mutate_path", path);
            let fails = if state_has("edit_fails") { state_get("edit_fails") } else { #{} };
            let f = fails[path] ?? 0;
            if f >= MAX_EDIT_FAILS {
                state_set("nudge", true);
                state_set("nudge_kind", "edit_fail");
                state_set("edit_fail_path", path);
                return #{
                    block: true,
                    reason: `edit/write on '${path}' already failed ${f} times. Re-read the full function, use edit_block with a larger SEARCH block, or safe_edit — do not retry the same failing patch.`
                };
            }
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
            return `<system-reminder> kind=edit_fail
Your edits to ${path} keep failing. Re-read the full function, then use edit_block (SEARCH/REPLACE) with a larger unique block, or safe_edit. Do not retry the same oldText.
</system-reminder>`;
        }
        if kind == "no_progress" {
            state_set("idle_rounds", 0);
            return `<system-reminder> kind=no_progress
You have made several tool calls without a successful file change or test run. Restate the goal in one sentence, then take ONE different concrete step.
</system-reminder>`;
        }
        return `<system-reminder> kind=repeat
You are repeating the same action. Re-read the error output and try a DIFFERENT approach.
</system-reminder>`;
    }
    if state_has("needs_verify") && state_get("needs_verify") {
        state_set("needs_verify", false);
        return `<system-reminder> kind=verify
You just modified files. Run the project's build and tests with bash before continuing. Do not claim done until they pass.
</system-reminder>`;
    }
    ()
}

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
                `<system-reminder> kind=stop_gate
STOP GATE: you edited files but have not shown a successful test/build run since. Run tests with bash now. Only finish after they pass (or report real failures).
</system-reminder>`
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
    "Plan updated. It will stay pinned via <system-reminder>."
}

fn on_context(messages) {
    // Drop prior plan pins only, re-append the active plan once near the tail.
    // Do not strip other <system-reminder> kinds — stop_gate / verify / thrash
    // nudges are injected as user messages and must remain model-visible.
    if !state_has("plan") {
        return messages;
    }
    let plan = state_get("plan");
    let filtered = messages.filter(|m| !is_plan_pin(m));
    let pin = #{
        role: "user",
        content: `<system-reminder> kind=plan
Current plan:
${plan}
[update with update_plan as you progress]
</system-reminder>`,
        timestamp: 0
    };
    let idx = if filtered.len() > 0 { filtered.len() - 1 } else { 0 };
    filtered.insert(idx, pin);
    filtered
}
