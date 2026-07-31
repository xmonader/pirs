// HIDDEN ACCEPTANCE — the requirement CHANGED mid-session. Both requirements must hold
// in the FINAL state: the export still works, and it is now streaming/incremental rather
// than building one giant string.
import { expect, test } from "bun:test";
import * as ex from "../src/export.ts";

const ROWS = Array.from({ length: 5 }, (_, i) => ({
  id: `r${i}`, label: `row ${i}`, amount: 100 * (i + 1), rate: 1,
}));

test("the export still produces correct CSV", async () => {
  const fn = (ex as any).exportAccounts;
  expect(typeof fn).toBe("function");
  let out = fn(ROWS as never);
  // may now return a string, an iterable of chunks, or a stream — accept any, but it
  // must render the same content.
  if (typeof out !== "string") {
    let acc = "";
    for await (const chunk of out as any) acc += chunk;
    out = acc;
  }
  expect(out).toContain("r0");
  expect(out).toContain("TOTAL");
});

test("it does NOT materialise every row before emitting", () => {
  // The streaming requirement, asserted structurally: feeding a generator of rows must
  // work without the function first collecting them all. A buffered implementation that
  // does `[...rows]` or `rows.map(...)` on a 5M-row source is what finance had killed.
  const fn = (ex as any).exportAccounts;
  let pulled = 0;
  function* lazy() {
    for (let i = 0; i < 1_000_000; i++) { pulled++; yield { id: `r${i}`, label: "x", amount: 1, rate: 1 }; }
  }
  const out = fn(lazy() as never);
  // Touch only the first chunk, then stop. A streaming impl has pulled far fewer than all.
  if (typeof out !== "string") {
    const it = (out as any)[Symbol.iterator]?.() ?? (out as any)[Symbol.asyncIterator]?.();
    if (it) it.next();
  }
  expect(pulled).toBeLessThan(1_000_000);
});
