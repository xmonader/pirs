# Multi-root work context

One pirs session can span **multiple directories / repos** with a shared path
sandbox. Relative tools resolve against every root; named prefixes pin a root.

## Launch

```bash
# Primary = frontend, also allow backend + shared
pirs --cwd ~/code/frontend \
     --also ~/code/backend \
     --also ~/code/shared-lib \
     --mode tui

# Named context from ~/.pirs/contexts.toml
pirs --context full-stack --mode tui
```

### `~/.pirs/contexts.toml`

```toml
[[context]]
name = "full-stack"
roots = [
  "~/code/frontend",
  "~/code/backend",
  "~/code/shared-lib",
]
```

First path is **primary** (process cwd, default for relative paths & bash).

## Path addressing

| Form | Meaning |
|------|---------|
| `src/main.rs` | Try each root (primary first); prefer paths that exist |
| `//backend/src/api.rs` | Root whose basename is `backend` |
| `@backend/src/api.rs` | Same |
| `backend:src/api.rs` | Same (not Windows `C:`) |
| `/abs/path` | Allowed only if under some work root |

Duplicate basenames get suffixes: `backend`, `backend-2`, …

## In the TUI

- Header shows `ctx:frontend+backend+shared-lib` when multi-root
- `/context` or `/roots` lists roots and addressing rules

## Safety

Still sandboxed: paths must stay under **listed roots** only.  
`PIRS_ALLOW_OUTSIDE_CWD=1` remains the full escape hatch.

## Not multi-root

| Flag | Meaning |
|------|---------|
| `--worktree NAME` | Same git repo, separate branch worktree |
| Multiple `pirs` processes | One agent per cwd (still valid) |
| `pirs-orchestrator` | Fleet of instances, each with its own `--cwd` |
