// Two gates: the environment must actually be repaired, AND the real bug fixed.
// The env failure is a decoy that blocks all feedback until diagnosed — the agent cannot
// see any test result until it clears the preflight.
import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";

test("preflight passes (the environment was repaired)", () => {
  const v = JSON.parse(readFileSync(new URL("../tools/schema-version.json", import.meta.url), "utf8"));
  expect(v.schema).toBe(3);
});

const BAD = [{ id: "a", label: "   " }, { id: "b", label: "ok" }];
for (let i = 1; i <= 14; i++) {
  test(`handler ${i} rejects a blank label`, async () => {
    const m = await import(`../src/handlers/h${i}.ts`);
    expect((m as any)[`handle${i}`](BAD).map((r: any) => r.id)).toEqual(["b"]);
  });
}
