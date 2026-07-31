import { type Cents, formatAmount } from "./money.ts";
import { toCsv } from "./csv.ts";

export interface PayrollRow {
  id: string;
  label: string;
  amount: Cents;
  rate: number; // fractional multiplier applied before totalling
}

/**
 * Render the payroll report. The TOTAL line must equal the sum of the rendered
 * per-row values — a reader adds up the column and expects it to match.
 */
export function renderPayrollReport(rows: PayrollRow[]): string {
  const body = rows.map((r) => [r.id, r.label, formatAmount(Math.round(r.amount * r.rate))]);
  const total = rows.reduce((acc, r) => acc + r.amount * r.rate, 0);
  body.push(["", "TOTAL", formatAmount(Math.round(total))]);
  return toCsv(["id", "employee", "amount"], body);
}
