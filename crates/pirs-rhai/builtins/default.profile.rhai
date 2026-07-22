// Built-in profile: default — session pack set when no --profile is given.
// Loads the full embedded catalog (`packs: "*"`). Override by placing
// `.pirs/profiles/default.rhai` (project) or `~/.pirs/profiles/default.rhai`
// (user), or pass `--profile <name|path>`.
//
// strategy is monolithic so interactive sessions are not forced into plan-exec;
// pair with --strategy / a custom profile when you want multi-phase runs.
#{
    name: "default",
    strategy: "monolithic",
    packs: "*",
}
