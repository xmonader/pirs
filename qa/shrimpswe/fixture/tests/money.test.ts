import { expect, test } from "bun:test";
import { parseAmount, formatAmount } from "../src/money.ts";

test("parses and formats", () => {
  expect(parseAmount("12.34")).toBe(1234);
  expect(parseAmount("-0.05")).toBe(-5);
  expect(formatAmount(1234)).toBe("12.34");
  expect(formatAmount(-5)).toBe("-0.05");
});
