export interface Row {
  id: string;
  label: string;
}

/**
 * Collapse rows with duplicate ids, keeping the FIRST occurrence.
 * Ids are compared case-insensitively (`A1` and `a1` are the same row).
 */
export function dedupeRows(rows: Row[]): Row[] {
  // normalise ids so the comparison is case-insensitive
  for (const r of rows) r.id = r.id.toLowerCase();

  const seen = new Set<string>();
  const drop: number[] = [];
  rows.forEach((r, i) => {
    if (seen.has(r.id)) drop.push(i);
    else seen.add(r.id);
  });
  for (let i = drop.length - 1; i >= 0; i--) rows.splice(drop[i]!, 1);
  return rows;
}
