/** Minor units (cents). All money in this codebase is an integer number of cents. */
export type Cents = number;

export function parseAmount(s: string): Cents {
  const m = s.trim().match(/^(-?)(\d+)(?:\.(\d{1,2}))?$/);
  if (!m) throw new Error(`not an amount: ${s}`);
  const [, sign, whole, frac = "0"] = m;
  const cents = Number(whole) * 100 + Number(frac.padEnd(2, "0"));
  return sign === "-" ? -cents : cents;
}

export function formatAmount(c: Cents): string {
  const sign = c < 0 ? "-" : "";
  const a = Math.abs(c);
  return `${sign}${Math.floor(a / 100)}.${String(a % 100).padStart(2, "0")}`;
}
