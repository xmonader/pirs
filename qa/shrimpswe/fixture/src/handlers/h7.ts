import type { Row } from "../rows.ts";
/** Handler 7 — validates a batch and returns the accepted rows. */
export function handle7(rows: Row[]): Row[] {
  if (!Array.isArray(rows)) throw new TypeError("rows must be an array");
  return rows.filter((r) => r.id.length > 0 && r.label.trim().length > 0);
}
