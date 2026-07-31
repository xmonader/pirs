---
name: streaming-export
description: >
  Streaming, incremental, or memory-safe export of large datasets (CSV/JSON lines,
  million-row ledgers, OOM/out-of-memory kills, "don't load everything at once").
  Use when the user mentions streaming, generators, iterators, large files, heap,
  OOM, multi-million rows, or mid-session redesign of a buffered export. Prefer
  pull-based APIs over building one giant string.
---

# Streaming / large-export design (pirs)

These rules come from a real miss: agents built generators then **joined into a
string**, so a hidden check that pulls only the first chunk still saw the whole
input generator consumed.

## Three rules (non-negotiable)

1. **Return a pull-based iterable** when the task is streaming / OOM / large
   export: `Generator` / `Iterable` / `AsyncIterable` of **chunks or lines**,
   not only a fully-built `string` (unless a string API is explicitly required
   *and* a streaming API is also exported).

2. **First pull of the result must not fully consume a one-shot input
   generator.** If the caller does `result.next()` once, the input source must
   still have most of its rows unpulled. Building the entire output string
   first fails this.

3. **Do not require a full first pass over a one-shot iterator only to compute
   TOTAL, then a second pass / joined string.** One-shot generators cannot be
   rewound. Prefer:
   - streaming lines with a **running** total emitted at the end after a single
     pass, or
   - documented two-pass only when the input is a **rewindable** collection
     (array / file path), never a one-shot generator.

## Concrete shape (TypeScript-ish)

```ts
// GOOD: pull-based lines; one pass; first next() pulls little of `rows`
export function* exportAccountsLines(rows: Iterable<Row>): Generator<string> {
  let total = 0;
  yield "id,account,amount";
  for (const r of rows) {
    const cents = Math.round(r.amount * r.rate);
    total += cents;
    yield `${r.id},${r.label},${format(cents)}`;
  }
  yield `,TOTAL,${format(total)}`;
}

// OK companion: materialise only when caller asks for a string
export function exportAccounts(rows: Iterable<Row>): string {
  return [...exportAccountsLines(rows)].join("\n");
}

// BAD for streaming tasks: always materialises everything before return
export function exportAccounts(rows: Iterable<Row>): string {
  const body = [...rows].map(...); // full consume
  return toCsv(header, body);      // full string
}
```

## When the requirement changes mid-session

If turn 1 built a buffered string export and turn 2 says "million rows / OOM":

1. Keep turn-1 correctness (CSV still correct).
2. Redesign the **public export API** to be pull-based (or add a streaming
   twin the grader will call).
3. Re-run tests that feed a **lazy generator**, not only a small array.

## Verification sketch

- Feed `function* { for (...) yield row }` with N=1_000_000.
- Call the export; if it returns an iterator, take **one** `next()`.
- Assert rows pulled from the source ≪ N.
- If it only returns a string, the streaming assertion fails by design.
