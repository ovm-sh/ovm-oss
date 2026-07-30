#!/usr/bin/env bash
set -euo pipefail

# Proves the npm postinstall (npm/ovm/install.js) replaces the live bundle
# transactionally: a failure during staging must leave the previous bundle
# fully usable, and a successful install must swap every file and drop obsolete
# binaries only after the new bundle is live.

node <<'NODE'
const fs = require("fs");
const os = require("os");
const path = require("path");

const { installBundle } = require(path.join(process.cwd(), "npm/ovm/install.js"));

function mkTemp(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

const dest = mkTemp("ovm-install-dest-");
const src = mkTemp("ovm-install-src-");

// Seed a previous LIVE bundle: ovm + ovm-old + its manifest.
const previousManifest = "ovm-bundle-v1\nmain\tovm\tovm\nside\tovm-old\tovm-old\n";
fs.writeFileSync(path.join(dest, "ovm"), "OLD-OVM");
fs.writeFileSync(path.join(dest, "ovm-old"), "OLD-SIDE");
fs.writeFileSync(path.join(dest, "ovm-bundle-v1.tsv"), previousManifest);

// A new bundle that renames the side binary ovm-old -> ovm-new.
const newManifestPath = path.join(src, "ovm-bundle-v1.tsv");
fs.writeFileSync(
  newManifestPath,
  "ovm-bundle-v1\nmain\tovm\tovm\nside\tovm-new\tovm-new\n",
);
fs.writeFileSync(path.join(src, "ovm"), "NEW-OVM");
fs.writeFileSync(path.join(src, "ovm-new"), "NEW-SIDE");
const entries = [
  { role: "main", binary: "ovm", cargoPackage: "ovm" },
  { role: "side", binary: "ovm-new", cargoPackage: "ovm-new" },
];

function assert(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
}

// 1. A failure mid-copy (source resolution throws) must abort with the previous
//    bundle fully intact and no temp files stranded.
let failed = false;
try {
  installBundle({
    entries,
    destDir: dest,
    manifestPath: newManifestPath,
    resolveSource: (binary) => {
      if (binary === "ovm-new") {
        throw new Error("injected copy failure");
      }
      return path.join(src, binary);
    },
  });
} catch (_error) {
  failed = true;
}
assert(failed, "a failing install must throw");
assert(fs.readFileSync(path.join(dest, "ovm"), "utf8") === "OLD-OVM", "ovm was mutated by a failed install");
assert(fs.readFileSync(path.join(dest, "ovm-old"), "utf8") === "OLD-SIDE", "ovm-old was removed by a failed install");
assert(
  fs.readFileSync(path.join(dest, "ovm-bundle-v1.tsv"), "utf8") === previousManifest,
  "the live manifest was swapped by a failed install",
);
for (const name of fs.readdirSync(dest)) {
  assert(!name.includes(".ovm-stage-"), `temp file leaked after failure: ${name}`);
}

// 2. A successful install swaps every file and removes the obsolete binary.
installBundle({
  entries,
  destDir: dest,
  manifestPath: newManifestPath,
  resolveSource: (binary) => path.join(src, binary),
});
assert(fs.readFileSync(path.join(dest, "ovm"), "utf8") === "NEW-OVM", "ovm was not updated");
assert(fs.readFileSync(path.join(dest, "ovm-new"), "utf8") === "NEW-SIDE", "ovm-new was not installed");
assert(!fs.existsSync(path.join(dest, "ovm-old")), "obsolete ovm-old was not removed");
for (const name of fs.readdirSync(dest)) {
  assert(!name.includes(".ovm-stage-"), `temp file leaked after success: ${name}`);
}

console.log("npm install transactional: OK");
NODE
