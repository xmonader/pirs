import { expect, test } from "bun:test";
import { toCsv, escapeCell } from "../src/csv.ts";

test("escapes per RFC4180", () => {
  expect(escapeCell('a,b')).toBe('"a,b"');
  expect(escapeCell('say "hi"')).toBe('"say ""hi"""');
  expect(toCsv(["a"], [["x"]])).toBe("a\nx");
});
