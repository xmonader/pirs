import type { Row } from "../rows.ts";
/** Handler 2 — validates a batch and returns the accepted rows. */
export function handle2(rows: Row[]): Row[] | null {
  if (!Array.isArray(rows)) return null;
  return rows.filter((r) => r.id.length > 0 && r.label.trim().length > 0);
}
