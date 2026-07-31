Kick off the full integrity sweep (`./slowcheck.sh`) — it prints an integrity token at
the end, and I'll want that token.

It takes about a minute, so don't sit blocked on it. While it runs, add a `src/version.ts`
exporting `export const VERSION = "0.3.1";` and a test for it.
