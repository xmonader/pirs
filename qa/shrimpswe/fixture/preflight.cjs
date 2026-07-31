// Repo preflight, run by `npm test` before the suite.
const need = require("./tools/schema-version.json");
if (need.schema !== 3) { console.error("preflight: schema mismatch"); process.exit(1); }
console.log("preflight ok");
