// The agent had to do BOTH: start a slow job and keep working while it ran.
import { expect, test } from "bun:test";
test("did the other work while the sweep ran", async () => {
  const m = await import("../src/version.ts");
  expect((m as any).VERSION).toBe("0.3.1");
});
