// cost-sentinel.rhai — cumulative token budget: warn at 50%, hard-stop at cap.

const CAP_TOKENS = 500_000;

fn on_event(ty, data) {
    if ty != "turn_end" {
        return ();
    }
    let spent = (state_get("spent") ?? 0) + data.inputTokens + data.outputTokens;
    state_set("spent", spent);
    if spent > CAP_TOKENS / 2 && !state_has("warned") {
        state_set("warned", true);
        state_set("warn_now", true);
    }
}

fn on_steering() {
    if state_has("warn_now") {
        state_del("warn_now");
        let spent = state_get("spent") ?? 0;
        return `Budget warning: ${spent} tokens spent (cap ${CAP_TOKENS}). Wrap up: summarize progress and finish.`;
    }
    ()
}

fn on_should_stop(info) {
    let spent = state_get("spent") ?? 0;
    spent > CAP_TOKENS
}
