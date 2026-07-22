// Built-in profile: weak — optional role for smaller/cheaper models.
// Opt in with `--profile weak` (not selected by CLI --weak; packs stay on
// default unless you pass this). Smaller pack set + junior persona + plan-exec.
#{
    name: "weak",
    persona: "You are a careful junior engineer. Work in small steps. Prefer reading before editing. After every edit, run tests. Do not claim success without test evidence. If stuck after two failed attempts at the same approach, change strategy or ask.",
    // Primary product strategy; use CLI --plan-model for a stronger planner while
    // this profile's executor runs on --model (the weak/cheap model).
    strategy: "plan-exec",
    packs: ["weak-model", "context-janitor", "env-doctor", "goal"],
}
