// HIDDEN ACCEPTANCE — turn 2 ran in a CLEAN workspace. The convention must come from MEMORY.
//
// The previous version of this task was inferable: the fresh copy still contained twelve
// handlers all using `throw new TypeError(...)`, which happened to be the convention turn 1
// "chose". Turn 2 simply read the siblings. It proved the agent can copy a dominant style —
// sensible, but not recall.
//
// Now turn 1 must INVENT a dedicated error type with a name of its own choosing. That name
// exists nowhere in the fresh workspace: not in src/errors.ts (absent), not in any sibling
// (they all still throw TypeError). If h2 references it, the name came from across the session
// boundary — there is no other source.
import { expect, test } from "bun:test";
import { readFileSync, existsSync } from "node:fs";

const h2 = readFileSync(new URL("../src/handlers/h2.ts", import.meta.url), "utf8");
const errorsPath = new URL("../src/errors.ts", import.meta.url);

test("turn 2 re-created the project's dedicated error type", () => {
  expect(existsSync(errorsPath)).toBe(true);
});

test("h2 uses that dedicated type, not a bare TypeError or null", () => {
  expect(/return\s+null/.test(h2)).toBe(false);
  const named = /throw new ([A-Z][A-Za-z0-9]*Error)\(/.exec(h2);
  expect(named).not.toBeNull();
  // TypeError is what every untouched sibling uses — copying it proves nothing.
  expect(named![1]).not.toBe("TypeError");
});

test("the name in h2 matches the one it defined", () => {
  const named = /throw new ([A-Z][A-Za-z0-9]*Error)\(/.exec(h2)!;
  const errs = readFileSync(errorsPath, "utf8");
  expect(errs).toContain(named[1]);
});
