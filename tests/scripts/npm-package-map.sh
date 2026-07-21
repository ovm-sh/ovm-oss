#!/usr/bin/env bash
set -euo pipefail

node <<'NODE'
const fs = require("fs");
const path = require("path");

const root = process.cwd();
const installer = fs.readFileSync(path.join(root, "npm/ovm/install.js"), "utf8");
const packageJson = JSON.parse(fs.readFileSync(path.join(root, "npm/ovm/package.json"), "utf8"));
const optionalDependencies = packageJson.optionalDependencies || {};
const manifest = fs
  .readFileSync(path.join(root, "crates/ovm/ovm-bundle-v1.tsv"), "utf8")
  .trim()
  .split("\n");

if (manifest.shift() !== "ovm-bundle-v1") {
  throw new Error("Unexpected bundle manifest header");
}
const bundleBinaries = manifest.map((line) => line.split("\t")[1]);
if (!bundleBinaries.includes("ovm")) {
  throw new Error("Bundle manifest has no ovm main binary");
}
if (!installer.includes('const MANIFEST_NAME = "ovm-bundle-v1.tsv"')) {
  throw new Error("npm installer does not consume the bundle manifest");
}

const packageNames = [...installer.matchAll(/"[^"]+":\s*"(@mochiexists\/ovm-[^"]+)"/g)].map(
  (match) => match[1],
);

if (packageNames.length === 0) {
  throw new Error("No platform package names found in npm/ovm/install.js");
}

for (const packageName of packageNames) {
  if (!Object.prototype.hasOwnProperty.call(optionalDependencies, packageName)) {
    throw new Error(`${packageName} is used by install.js but missing from optionalDependencies`);
  }
}

for (const packageName of Object.keys(optionalDependencies)) {
  if (!packageNames.includes(packageName)) {
    throw new Error(`${packageName} is optional but not used by install.js`);
  }
}

for (const platformDir of [
  "ovm-darwin-arm64",
  "ovm-darwin-x64",
  "ovm-linux-arm64",
  "ovm-linux-x64",
]) {
  const platformPackage = JSON.parse(
    fs.readFileSync(path.join(root, "npm", platformDir, "package.json"), "utf8"),
  );
  const files = platformPackage.files || [];
  if (!files.includes("ovm") || !files.includes("ovm-*")) {
    throw new Error(`${platformDir} does not package the dynamic bundle files`);
  }
}

console.log(
  `npm-package-map: ok (${packageNames.length} platforms, ${bundleBinaries.length} bundle binaries)`,
);
NODE
