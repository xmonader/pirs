// review-gate.rhai — independent reviewer that can REFUSE completion.
//
// After mutations, host `review_report` builds a deterministic plan:
//   file list, risk-ranked units, content-hash cache, structured findings
//   (added lines only + slices), residual-risk cascade, hysteresis.
// LLM subagent runs only when CASCADE:needs_llm (and not HYSTERESIS:skip).
// Structured AUTO_BLOCK (error findings) always blocks.
//
// state_set("reviewer_model", "…") overrides the subagent model.

const REVIEW_PROMPT = `You are an independent, adversarial code reviewer who did NOT write this change.
Judge the change against the USER'S REQUEST (not just tests) and attack it for bugs,
edge cases, race conditions, and broken invariants.

You receive a DETERMINISTIC REVIEW PLAN: complete file list, risk-ranked units,
read-only tool diet, structured findings with local slices, and a capped diff of
top-risk units only. Trust the file list as complete for this turn.

Flag as CRITICAL:
- The change does not actually do what was asked (wrong behavior, wrong semantics)
- Test-gaming: weakened assertions, narrowed selectors, deleted cases, tautological tests
- A bug that will fail in real use (edge cases, invariants, error paths, race conditions)

Otherwise reply SOUND. Format: first line exactly "CRITICAL" or "SOUND", then bullets.
Do not request write/edit/bash tools — observation only.`;

fn on_tool_result(id, name, result) {
    if (name == "edit" || name == "write" || name == "edit_block" || name == "ast_edit") && !result.isError {
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

    let lines = review_report("");
    if lines.len() == 0 {
        return ();
    }
    let head = `${lines[0]}`;
    if head.starts_with("error:") {
        state_set("review_blocked", head);
        return `REVIEW BLOCKED — could not build deterministic review plan:\n${head}\n\nFix the workspace/git state and re-verify.`;
    }
    let ctx = if lines.len() > 1 { `${lines[1]}` } else { head };

    // Structured error findings — always block (precision path).
    if ctx.contains("AUTO_BLOCK:") {
        state_set("review_blocked", ctx);
        return `REVIEW BLOCKED — structured review found error-severity issues:\n${ctx}\n\nYou MUST fix the flagged issues and re-verify. The run cannot finish while the review is failing.`;
    }

    if ctx.contains("files: 0") {
        state_del("review_blocked");
        return ();
    }

    // Same plan fingerprint within hysteresis window — skip re-review thrash.
    if ctx.contains("HYSTERESIS:skip") {
        return ();
    }

    // Cascade: low residual risk → no LLM (cheap path).
    if ctx.contains("CASCADE:skip_llm") {
        state_del("review_blocked");
        return ();
    }

    let request = state_get("first_request") ?? "(unspecified task)";
    let model = state_get("reviewer_model") ?? "";
    let verdict = run_subagent(
        `${REVIEW_PROMPT}\n\nUSER REQUEST:\n${request}\n\n${ctx}`,
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
