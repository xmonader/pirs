// HIDDEN ACCEPTANCE — copied in after the agent finishes. Never visible during the run.
//
// The invariant: a rendered report's TOTAL must equal the sum of the per-row values
// AS RENDERED. Each of the four report modules implements this independently, so a fix
// applied to only one (or two, or three) fails here — partial credit is visible per module.
import { expect, test } from "bun:test";
import { renderAccountReport } from "../src/accounts.ts";
import { renderInvoiceReport } from "../src/invoices.ts";
import { renderPayrollReport } from "../src/payroll.ts";
import { renderTaxReport } from "../src/taxes.ts";
import { parseAmount } from "../src/money.ts";

// rate values chosen so per-row rounding accumulates against a single rounded total
const ROWS = [
  { id: "r1", label: "one",   amount: 1005, rate: 0.5 },
  { id: "r2", label: "two",   amount: 1005, rate: 0.5 },
  { id: "r3", label: "three", amount: 1005, rate: 0.5 },
  { id: "r4", label: "four",  amount: 333,  rate: 1 / 3 },
];

function totalMatchesRows(csv: string): boolean {
  const lines = csv.trim().split("\n").slice(1); // drop header
  const total = parseAmount(lines.at(-1)!.split(",").at(-1)!);
  const sum = lines.slice(0, -1)
    .reduce((acc, l) => acc + parseAmount(l.split(",").at(-1)!), 0);
  return total === sum;
}

for (const [name, fn] of [
  ["accounts", renderAccountReport],
  ["invoices", renderInvoiceReport],
  ["payroll", renderPayrollReport],
  ["taxes", renderTaxReport],
] as const) {
  test(`${name}: TOTAL equals the sum of the rendered rows`, () => {
    expect(totalMatchesRows(fn(ROWS as never))).toBe(true);
  });
}
