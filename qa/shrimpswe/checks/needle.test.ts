import { expect, test } from "bun:test";
const BAD = [{ id: "a", label: "   " }, { id: "b", label: "ok" }];
for (let i = 1; i <= 14; i++) {
  test(`handler ${i} rejects a blank label`, async () => {
    const m = await import(`../src/handlers/h${i}.ts`);
    expect((m as any)[`handle${i}`](BAD).map((r: any) => r.id)).toEqual(["b"]);
  });
}
