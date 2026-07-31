Please tidy up `dedupeRows` in `src/rows.ts` — it works but it's hard to follow.

Keep the behaviour identical: collapse rows with duplicate ids keeping the first
occurrence, matching ids case-insensitively.
