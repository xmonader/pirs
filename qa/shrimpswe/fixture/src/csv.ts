/**
 * CSV writer. Callers pass a header row and data rows; every cell is escaped
 * per RFC 4180 (quote-wrap on comma/quote/newline, inner quotes doubled).
 */
export function escapeCell(value: unknown): string {
  const s = value == null ? "" : String(value);
  if (s.includes(",") || s.includes('"') || s.includes("\n")) {
    return '"' + s.replace(/"/g, '""') + '"';
  }
  return s;
}

export function toCsv(header: string[], rows: unknown[][]): string {
  const lines = [header.map(escapeCell).join(",")];
  for (const r of rows) lines.push(r.map(escapeCell).join(","));
  return lines.join("\n");
}
