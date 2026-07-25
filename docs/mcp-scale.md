# MCP at scale (catalog + lazy pool)

pirs does **not** connect every configured MCP server at session start when the
catalog is large. That would spawn unbounded stdio processes and flood the
model tool schema.

## Policy

| Configured servers | Behavior |
|--------------------|----------|
| **0** | No MCP tools |
| **≤ `PIRS_MCP_EAGER_MAX`** (default **8**) | **Eager** — connect and flatten remote tools into the agent schema (compat with small `.mcp.json`) |
| **> eager max** | **Catalog-router** — only 6 meta tools; connect on demand |

### Router tools (catalog mode)

| Tool | Role |
|------|------|
| `mcp_search` | Search catalog by name/tag/tool (no connect-all) |
| `mcp_describe` | Schema for one tool (may connect that server) |
| `mcp_call` | Invoke `server` + `tool` + `arguments` (lazy connect) |
| `mcp_enable` | Warm one server into the live pool |
| `mcp_disable` | Drop a live connection (frees a slot) |
| `mcp_status` | catalog size vs live/max |

### Live pool cap

| Env | Default | Meaning |
|-----|---------|---------|
| `PIRS_MCP_MAX_LIVE` | 16 | Max concurrent live MCP clients |
| `PIRS_MCP_EAGER_MAX` | 8 | Threshold for eager flatten |
| `PIRS_MCP_FORCE_ROUTER` | — | Always use catalog-router (even for 1 server) |
| `PIRS_MCP_FORCE_EAGER` | — | Always connect-all (unsafe at scale) |

Exceeding the live cap **fails closed** with a clear error until you
`mcp_disable` another server or raise `PIRS_MCP_MAX_LIVE`.

## Large fleets

List thousands of servers in config or a catalog index — **metadata only** until
`mcp_enable` / `mcp_call`. Prefer HTTP transports or a single gateway URL when
possible; stdio is process-heavy.

```bash
# Force router for a single mock while testing
export PIRS_MCP_FORCE_ROUTER=1
export PIRS_MCP_MAX_LIVE=4
pirs --doctor   # shows mcp scale lines + PIRS_MCP_DOCTOR_LINES after a session load
```

## Small configs (unchanged happy path)

```json
{
  "mcpServers": {
    "email-calendar": {
      "command": "python3",
      "args": ["…/mcp_email_calendar.py"]
    }
  }
}
```

With one server, eager mode still lists `mcp_email-calendar_email_list` etc.
as top-level tools. See [mcp-email-calendar.md](mcp-email-calendar.md).
