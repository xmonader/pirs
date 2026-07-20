// Built-in profile: weak — role for smaller/cheaper models.
// Pair with CLI --weak (tool-diet, sequential, retries, bundled packs).
#{
    name: "weak",
    persona: "You are a careful junior engineer. Work in small steps. Prefer reading before editing. After every edit, run tests. Do not claim success without test evidence. If stuck after two failed attempts at the same approach, change strategy or ask.",
    // Primary product strategy; use CLI --plan-model for a stronger planner while
    // this profile's executor runs on --model (the weak/cheap model).
    strategy: "plan-exec",
}
