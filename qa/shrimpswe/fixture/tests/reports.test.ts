import { expect, test } from "bun:test";
import { renderAccountReport } from "../src/accounts.ts";
import { renderInvoiceReport } from "../src/invoices.ts";

// Shape only — these do NOT check total consistency.
test("renders a header and a TOTAL line", () => {
  const out = renderAccountReport([{ id: "a1", label: "Cash", amount: 1000, rate: 1 }]);
  const lines = out.split("\n");
  expect(lines[0]).toBe("id,account,amount");
  expect(lines.at(-1)).toBe(",TOTAL,10.00");
});

test("invoice report renders rows", () => {
  const out = renderInvoiceReport([{ id: "i1", label: "ACME", amount: 500, rate: 1 }]);
  expect(out).toContain("i1,ACME,5.00");
});

// Visible test: checks only the RETURNED value.
import { dedupeRows } from "../src/rows.ts";
test("dedupeRows keeps the first of each id", () => {
  const out = dedupeRows([
    { id: "a1", label: "first" },
    { id: "A1", label: "dup" },
    { id: "b2", label: "other" },
  ]);
  expect(out.map((r) => r.label)).toEqual(["first", "other"]);
});
