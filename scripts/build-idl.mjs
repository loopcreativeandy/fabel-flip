// Builds idl/coinflip.json from the program source without the anchor CLI.
//
// Anchor's `idl-build` feature generates hidden tests that print IDL
// fragments; this script runs them (with PDA seed resolution enabled via
// ANCHOR_IDL_BUILD_RESOLUTION) and assembles the fragments into a full IDL,
// the same way `anchor idl build` does.
//
// Run from the repo root: node scripts/build-idl.mjs
import { execSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";

const raw = execSync(
  "cargo test -p coinflip --features idl-build __anchor_private_print_idl -- --show-output --quiet",
  { env: { ...process.env, ANCHOR_IDL_BUILD_RESOLUTION: "TRUE" }, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] },
);

const chunks = [...raw.matchAll(/--- IDL begin (\w+) ---\n([\s\S]*?)\n--- IDL end \1 ---/g)];
let program, address, errors = [], constants;
const events = [], eventTypes = [];
for (const [, kind, body] of chunks) {
  const json = JSON.parse(body);
  if (kind === "program") program = json;
  else if (kind === "address") address = JSON.parse(json); // double-encoded string
  else if (kind === "errors") errors = json;
  else if (kind === "constants") constants = json;
  else if (kind === "event") { events.push(json.event); eventTypes.push(...json.types); }
}
if (!program) throw new Error("no program IDL chunk found in cargo test output");

// Strip Rust module paths from names (coinflip::Bet -> Bet), recursively,
// matching what the anchor CLI produces.
const fix = (o) => {
  if (Array.isArray(o)) o.forEach(fix);
  else if (o && typeof o === "object") {
    if (typeof o.name === "string" && o.name.includes("::")) o.name = o.name.split("::").pop();
    Object.values(o).forEach(fix);
  }
};

const idl = {
  ...program,
  address,
  metadata: {
    name: "coinflip",
    version: "0.1.0",
    spec: "0.1.0",
    description: "Provably-fair SOL coinflip with a 51/49 house edge",
  },
  events,
  errors,
  ...(constants ? { constants } : {}),
};
idl.types = idl.types ?? [];
const seen = new Set(idl.types.map((t) => t.name));
for (const t of eventTypes) if (!seen.has(t.name) && seen.add(t.name)) idl.types.push(t);
fix(idl);

mkdirSync("idl", { recursive: true });
writeFileSync("idl/coinflip.json", JSON.stringify(idl, null, 2) + "\n");
console.log(
  `idl/coinflip.json: address ${idl.address}, ${idl.instructions.length} instructions, ` +
    `${idl.accounts.length} accounts, ${idl.events.length} events, ${idl.errors.length} errors`,
);
