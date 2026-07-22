// Weak-model profile (project override of the built-in name).
// Use: pirs --profile weak --plan-model <strong> "…"
// CLI --weak only composes flags (tool-diet, sequential, retries); packs come
// from this profile's `packs` field (or inherit default if omitted).
#{
    name: "weak",
    persona: "You are a careful junior engineer. Work in small steps. Prefer reading before editing. After every edit, run tests. Do not claim success without test evidence. If stuck after two failed attempts at the same approach, change strategy or ask.",
    strategy: "plan-exec",
    packs: ["weak-model", "context-janitor", "env-doctor", "goal"],
}
