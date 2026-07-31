import type { Row } from "../rows.ts";
/** Handler 9 — validates a batch and returns the accepted rows. */
export function handle9(rows: Row[]): Row[] {
  if (!Array.isArray(rows)) throw new TypeError("rows must be an array");
  return rows.filter((r) => r.id.trim().length > 0 && r.label.length > 0);
}
