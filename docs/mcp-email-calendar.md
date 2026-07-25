# Email & calendar via MCP

pirs does **not** ship in-process Gmail/Outlook/Google Calendar OAuth clients.
Connectors are **MCP servers** you configure; tools load into the agent tool set
and appear in doctor/status.

## Config paths

| Path | Scope |
|------|--------|
| `{cwd}/.mcp.json` | Project (trusted interactively on first load) |
| `~/.pirs/mcp.json` | User-global |

Format (stdio example):

```json
{
  "mcpServers": {
    "email-calendar": {
      "command": "python3",
      "args": ["/absolute/path/to/mcp_email_calendar.py"]
    }
  }
}
```

HTTP/SSE servers use `"url": "https://…"` instead of `command`/`args`.

Env interpolation: `${VAR}` in command/args/url/headers; `!shell-command` for
secret material from a password manager CLI.

## Mock connector (no credentials)

In-repo mock for tests and local smoke:

```text
crates/pirs-mcp/tests/mcp_email_calendar.py
```

Tools exposed:

| Tool | Role |
|------|------|
| `email_list` | list inbox (mock) |
| `email_read` | read message by id |
| `calendar_list` | list events |
| `calendar_get` | get event by id |

Agent tool names are prefixed: `mcp_<server>_<tool>`  
e.g. `mcp_email-calendar_email_list`.

### Smoke

```bash
# Point project config at the mock
cat > .mcp.json <<EOF
{
  "mcpServers": {
    "email-calendar": {
      "command": "python3",
      "args": ["$(pwd)/crates/pirs-mcp/tests/mcp_email_calendar.py"]
    }
  }
}
EOF

# Integration test (no live mail):
cargo test -p pirs-mcp email_calendar -- --nocapture

# Doctor / status surfaces MCP health after load:
pirs --doctor
# or in-session: tool doctor
```

## Real providers

Use any MCP server that speaks tools/list + tools/call (stdio or HTTP), for
example community Gmail/Calendar MCP packages. Put OAuth tokens in env vars
referenced from `~/.pirs/mcp.json` — never commit secrets.

## Product boundary

- **In scope:** load, invoke, doctor lines, mock proof.
- **Out of scope:** bundled OAuth product, provider-specific UI, channel zoo.

## Large catalogs

Many servers use **catalog + lazy pool** (not connect-all). See [mcp-scale.md](mcp-scale.md).
