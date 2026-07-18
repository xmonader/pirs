// review-gate.rhai — an independent reviewer that can REFUSE completion.
// Every edit/write turn is reviewed (against the original request, not just
// tests). A Critical verdict blocks the run until the issues are fixed and
// the review passes. Reviewer model defaults to the session model; set
// state_set("reviewer_model", "other-model") to force a different one.

const REVIEW_PROMPT = `You are an independent code reviewer who did NOT write this change. Judge the diff against the USER'S REQUEST, not just the tests (a gamed change passes tests and is still wrong).

Flag as CRITICAL:
- The change does not actually do what was asked (wrong behavior, wrong semantics)
- Test-gaming: weakened assertions, narrowed selectors, deleted cases, tautological tests
- A bug that will fail in real use (edge cases, invariants, error paths)

Otherwise reply SOUND. Format: first line exactly "CRITICAL" or "SOUND", then bullets.`;

fn on_tool_result(id, name, result) {
    if (name == "edit" || name == "write") && !result.isError {
        state_set("needs_review", true);
    }
    ()
}

fn on_context(messages) {
    if !state_has("first_request") && messages.len() > 0 {
        for m in messages {
            if m.role == "user" && !m.content.to_string().starts_with("[") {
                state_set("first_request", m.content.to_string());
                break;
            }
        }
    }
    messages
}

fn on_follow_up() {
    if !state_has("needs_review") {
        return ();
    }
    state_del("needs_review");
    let diff = exec("git diff HEAD 2>/dev/null | head -c 9000", 15).output;
    let d = diff;
    d.trim();
    if d == "" {
        return ();
    }
    let request = state_get("first_request") ?? "(unspecified task)";
    let model = state_get("reviewer_model") ?? "";
    let verdict = run_subagent(
        `${REVIEW_PROMPT}\n\nUSER REQUEST:\n${request}\n\nDIFF:\n${d}`,
        model
    );
    if verdict.contains("CRITICAL") {
        state_set("review_blocked", verdict);
        return `REVIEW BLOCKED — the independent reviewer rejected this change:\n${verdict}\n\nYou MUST fix the flagged issues and re-verify. The run cannot finish while the review is failing.`;
    }
    state_del("review_blocked");
    ()
}

fn on_should_stop(info) {
    state_has("review_blocked")
}
