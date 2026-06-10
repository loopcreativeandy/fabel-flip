// Generates the typed @solana/kit client in examples/generated/ from the
// Anchor IDL. Run with: node codama.mjs
import { createFromRoot } from "codama";
import { rootNodeFromAnchor } from "@codama/nodes-from-anchor";
import { renderVisitor } from "@codama/renderers-js";
import { existsSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { globSync } from "node:fs";
import { dirname, resolve } from "node:path";

const idl = JSON.parse(readFileSync(new URL("../idl/coinflip.json", import.meta.url), "utf8"));
const codama = createFromRoot(rootNodeFromAnchor(idl));
await codama.accept(renderVisitor("./generated"));

// Drop the package scaffold so the files inherit this package's ESM mode;
// the deps it would declare are already in examples/package.json.
rmSync("./generated/package.json", { force: true });

// Node ESM needs explicit extensions; the renderer emits extensionless
// relative imports, which silently break star re-exports under tsx.
for (const file of globSync("./generated/**/*.ts")) {
  const src = readFileSync(file, "utf8");
  const fixed = src.replace(/(from\s+")(\.{1,2}\/[^"]+)(")/g, (m, pre, spec, post) => {
    if (/\.[cm]?js$/.test(spec)) return m;
    const base = resolve(dirname(file), spec);
    if (existsSync(`${base}.ts`)) return `${pre}${spec}.js${post}`;
    if (existsSync(`${base}/index.ts`)) return `${pre}${spec}/index.js${post}`;
    return m;
  });
  if (fixed !== src) writeFileSync(file, fixed);
}
console.log("generated client in examples/generated/");
