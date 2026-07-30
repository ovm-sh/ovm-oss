#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");

const PLATFORMS = {
  "darwin-arm64": "@mochiexists/ovm-darwin-arm64",
  "darwin-x64": "@mochiexists/ovm-darwin-x64",
  "linux-arm64": "@mochiexists/ovm-linux-arm64",
  "linux-x64": "@mochiexists/ovm-linux-x64",
};

const MANIFEST_NAME = "ovm-bundle-v1.tsv";
const SAFE_BINARY = /^ovm(?:-[a-z0-9]+)*$/;
const SAFE_PACKAGE = /^(?:-|[a-z0-9]+(?:-[a-z0-9]+)*)$/;

function parseManifest(contents) {
  const lines = contents.replace(/\r\n/g, "\n").split("\n");
  if (lines.at(-1) === "") {
    lines.pop();
  }
  if (lines.shift() !== "ovm-bundle-v1") {
    throw new Error("unsupported or missing bundle manifest header");
  }

  const entries = [];
  const binaries = new Set();
  const packages = new Set();
  let mainCount = 0;

  for (const [index, line] of lines.entries()) {
    const fields = line.split("\t");
    if (fields.length !== 3) {
      throw new Error(`bundle manifest line ${index + 2} must have three fields`);
    }

    const [role, binary, cargoPackage] = fields;
    if (role !== "main" && role !== "side") {
      throw new Error(`bundle manifest line ${index + 2} has invalid role ${role}`);
    }
    if (!SAFE_BINARY.test(binary)) {
      throw new Error(`bundle manifest line ${index + 2} has unsafe binary ${binary}`);
    }
    if (!SAFE_PACKAGE.test(cargoPackage)) {
      throw new Error(`bundle manifest line ${index + 2} has unsafe package ${cargoPackage}`);
    }
    if (binaries.has(binary)) {
      throw new Error(`bundle manifest repeats binary ${binary}`);
    }
    if (cargoPackage !== "-" && packages.has(cargoPackage)) {
      throw new Error(`bundle manifest repeats package ${cargoPackage}`);
    }

    binaries.add(binary);
    if (cargoPackage !== "-") {
      packages.add(cargoPackage);
    }
    if (role === "main") {
      mainCount += 1;
      if (binary !== "ovm" || cargoPackage !== "ovm") {
        throw new Error("bundle main row must be main<TAB>ovm<TAB>ovm");
      }
    }
    entries.push({ role, binary, cargoPackage });
  }

  if (mainCount !== 1) {
    throw new Error("bundle manifest must contain exactly one main row");
  }
  return entries;
}

// Install the bundle transactionally: stage every replacement under a temp
// name in the destination dir, validate it, then swap them all into place with
// rename (atomic per file), and ONLY THEN remove binaries the new manifest
// dropped. A failure anywhere before the swap phase leaves the previous live
// bundle fully intact — a mid-copy crash can never strand a partial bundle that
// existing shims fail to launch.
//
// `resolveSource(binary)` returns the on-disk path of a staged file; injected
// by tests, defaults to resolving the platform package's binaries.
function installBundle({ entries, destDir, manifestPath, resolveSource }) {
  fs.mkdirSync(destDir, { recursive: true });

  const installedManifest = path.join(destDir, MANIFEST_NAME);
  // Read the currently-installed manifest FIRST so we know which binaries the
  // new bundle drops — but delete nothing until the new bundle is live.
  let previousEntries = [];
  if (fs.existsSync(installedManifest)) {
    previousEntries = parseManifest(fs.readFileSync(installedManifest, "utf8"));
  }

  const suffix = `.ovm-stage-${process.pid}`;
  const staged = [];
  const cleanupStaged = () => {
    for (const { temp } of staged) {
      fs.rmSync(temp, { force: true });
    }
  };

  try {
    // Resolve, copy, permission, and validate every replacement before any
    // swap. Any failure here throws with the previous bundle untouched.
    for (const entry of entries) {
      const source = resolveSource(entry.binary);
      const temp = path.join(destDir, `${entry.binary}${suffix}`);
      fs.copyFileSync(source, temp);
      fs.chmodSync(temp, 0o755);
      if (!fs.statSync(temp).isFile()) {
        throw new Error(`staged ${entry.binary} is not a regular file`);
      }
      staged.push({ temp, final: path.join(destDir, entry.binary) });
    }
    const manifestTemp = path.join(destDir, `${MANIFEST_NAME}${suffix}`);
    fs.copyFileSync(manifestPath, manifestTemp);
    staged.push({ temp: manifestTemp, final: installedManifest });

    // Swap phase: rename is atomic per file and every source is already
    // validated on disk, so this cannot leave a partial live bundle.
    for (const { temp, final } of staged) {
      fs.renameSync(temp, final);
    }
  } catch (error) {
    cleanupStaged();
    throw error;
  }

  // The new bundle is fully live; drop obsolete binaries LAST.
  const nextNames = new Set(entries.map((entry) => entry.binary));
  for (const entry of previousEntries) {
    if (!nextNames.has(entry.binary)) {
      fs.rmSync(path.join(destDir, entry.binary), { force: true });
    }
  }
}

function main() {
  const platform = `${process.platform}-${process.arch}`;
  const pkg = PLATFORMS[platform];

  if (!pkg) {
    console.error(`ovm: unsupported platform ${platform}`);
    process.exit(1);
  }

  const destDir = path.join(__dirname, "bin");

  try {
    const manifestPath = require.resolve(`${pkg}/${MANIFEST_NAME}`);
    const entries = parseManifest(fs.readFileSync(manifestPath, "utf8"));
    installBundle({
      entries,
      destDir,
      manifestPath,
      resolveSource: (binary) => require.resolve(`${pkg}/${binary}`),
    });
  } catch (error) {
    console.error(`ovm: failed to install binary bundle: ${error.message}`);
    console.error("You can install from source instead: cargo install ovm");
    process.exit(1);
  }
}

module.exports = { parseManifest, installBundle };

if (require.main === module) {
  main();
}
