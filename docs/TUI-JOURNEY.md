# First-time TUI journey (low friction)

This is the shortest path from zero to a useful coding session in the pirs
terminal UI. Total time: **about one minute**.

## Before you open the TUI

```bash
# 1. API key for your provider (OpenAI-compatible or Anthropic)
export OPENAI_API_KEY=...          # or ANTHROPIC_API_KEY / DASHSCOPE_API_KEY / …

# 2. Build once
cargo build --release -p pirs

# 3. Enter a project you care about
cd /path/to/your/repo
```

Optional but nice:

```bash
# Safer first session: ask before shell/writes
export PIRS_APPROVAL=ask

# Or explore read-only first
# (you can also type /plan inside the TUI)
```

## Open the console

```bash
./target/release/pirs --mode tui
# or, if installed:
pirs --mode tui
```

**First launch** shows a short **Getting started** panel (stored in
`~/.pirs/tui_onboarded` so it only appears once). Re-show anytime with
`/tour`.

Force the welcome again: `PIRS_TUI_FORCE_ONBOARD=1 pirs --mode tui`  
Skip it: `PIRS_TUI_SKIP_ONBOARD=1 pirs --mode tui`

---

## The 60-second path

### Step 1 — Pick a starter (or type your own)

With the input **empty**, press:

| Key | Fills the input with… |
|-----|------------------------|
| `1` | Explain this repository |
| `2` | Run tests & fix failures |
| `3` | Review uncommitted changes |

Then press **Enter** to send.  
Or type any goal in plain English and press Enter.

> Tip: **alt+enter** (or **ctrl-j**) inserts a newline without sending.

### Step 2 — Watch the agent work

You will see:

- `│ assistant` streaming text  
- Tool cards: `✓ Read path` · `⠹ Running bash …` · `✓ Ran bash …`  
- Quiet tools fold into groups: **`✓ Read 3 files`** (Tab expands)  
- Edits show **green `+` / red `-`** lines when expanded  
- Status row: spinner · activity · elapsed · `esc cancel`

**While it runs**, type more text and Enter to **steer** the turn.  
**Esc** or **ctrl-c** cancels.

### Step 3 — Approvals (if asked)

If approval mode is `ask`, a panel appears:

| Key | Meaning |
|-----|---------|
| `y` | Yes, once |
| `a` | Always this session |
| `n` / `esc` | Deny |

There is a short grace period so Enter from your previous message cannot
auto-confirm.

### Step 4 — Learn three shortcuts

| Key | Action |
|-----|--------|
| `?` | Help overlay |
| `/` then **Tab** | Slash command completion |
| `1` `2` `3` | Starters (empty input) |

Useful slash commands:

| Command | Why |
|---------|-----|
| `/tour` | Show the first-run panel again |
| `/plan` | Read-only explore (safe) |
| `/act` | Allow writes + full tools |
| `/model` | **Fuzzy model picker** (or `/model backend/id`) |
| `/models refresh` | Pull provider catalogs into the picker |
| `/stats` | Tokens + timing |
| `/undo` | Rewind last user turn |
| `/clear` | Clear the screen |
| `/quit` | Exit |

Shell without the agent: `!cargo test` (records in context) or `!!cargo test`
(no record).

---

## Recommended first sessions

### A. “What is this repo?” (safe)

1. `pirs --mode tui`  
2. Press `1` → Enter  
3. Read the summary; ask follow-ups  

### B. “Fix the tests” (coding)

1. Start with `/plan` if you want a dry look first, then `/act`  
2. Press `2` → Enter  
3. Approve bash when prompted (`y` or `a`)  
4. When green, press `3` to review the diff  

### C. Strong plan / cheap exec (multi-model)

```bash
pirs --mode tui \
  --model qwen-plus \
  --plan-model deepseek-v4-flash \
  --strategy plan-exec
```

Type a hard task; planning uses `--plan-model`, execution uses `--model`.

---

## How the chrome maps to “what do I do next?”

```
┌ header ──── pirs │ model │ ● approval │ ~/project ──────────────┐
│ chat     assistant + tools (collapse noise, expand detail)       │
│ status   ⠹ Running bash · 12s              in 12k · esc cancel   │
└ input ── ❯ type a goal · / for commands · 1–3 starters ─────────┘
```

- **Idle status** → type a goal or press `1`–`3`  
- **Spinner** → wait, or type to steer, or esc cancel  
- **◆ approval** → `y` / `a` / `n`  
- **Rose / green / amber input border** → yolo / plan / busy modes  

Theme: `PIRS_TUI_THEME=mono pirs --mode tui` for limited terminals.

---

## Friction we deliberately removed

| Old friction | Now |
|--------------|-----|
| Blank screen, unclear next step | First-run panel + starters `1`–`3` |
| Memorize slash commands | Type `/` + Tab / ↑↓ completion |
| Tool spam (50 reads) | Verb groups: “Read 3 files” |
| Raw JSON tool args | Human verbs + path/command |
| Accidental approval on Enter | 400ms grace + overlay |
| Lost help | `?` and `/tour` always available |
| “What mode am I in?” | Mode-colored composer + header ● |

---

## After your first success

1. Add model aliases in `~/.pirs/config.toml` (see README).  
2. Prefer `--approval ask` until you trust the setup, then `auto`.  
3. Use `/strategy plan-exec` for larger refactors.  
4. Sessions live under `~/.pirs/sessions/` (`--resume`).  
5. Daily chat + gateway work is **`pirs-claw`** — same tools, different product surface.

If something feels stuck: `/doctor`, then `/help`.
