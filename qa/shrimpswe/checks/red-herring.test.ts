// HIDDEN ACCEPTANCE — copied in after the agent finishes.
//
// TWO traps, deliberately. `dedupeRows` mutates the caller's data in two separate ways:
//   (1) it splices the ARRAY in place
//   (2) it lower-cases each row's `id` IN PLACE, before any deduping
// A fix that only copies the array (`rows = [...rows]`) still corrupts the caller's row
// OBJECTS and fails here. This is the "fix EVERY site of the bug" failure mode with an
// invariant no visible test asserts.
import { expect, test } from "bun:test";
import { dedupeRows, type Row } from "../src/rows.ts";

function input(): Row[] {
  return [
    { id: "A1", label: "first" },
    { id: "a1", label: "dup" },
    { id: "B2", label: "other" },
  ];
}

test("still returns the correct unique set, first occurrence wins", () => {
  expect(dedupeRows(input()).map((r) => r.label)).toEqual(["first", "other"]);
});

test("does not mutate the caller's ARRAY", () => {
  const rows = input();
  dedupeRows(rows);
  expect(rows.length).toBe(3);
});

test("does not mutate the caller's ROW OBJECTS", () => {
  const rows = input();
  dedupeRows(rows);
  expect(rows.map((r) => r.id)).toEqual(["A1", "a1", "B2"]);
});

test("case-insensitive matching still works", () => {
  expect(dedupeRows([{ id: "x", label: "a" }, { id: "X", label: "b" }]).length).toBe(1);
});
