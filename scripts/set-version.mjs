// Stamp / bump the version across minibucket's manifests.
//
//   node scripts/set-version.mjs 0.2.0     # set an explicit version
//   node scripts/set-version.mjs patch     # bump 0.1.4 -> 0.1.5
//   node scripts/set-version.mjs minor     # bump 0.1.4 -> 0.2.0
//   node scripts/set-version.mjs major     # bump 0.1.4 -> 1.0.0
//
// Patches Cargo.toml (the [package] version) and Cargo.lock (minibucket's own
// package entry) via targeted regex replaces, so existing formatting and
// comments are left untouched. minibucket is dependency-free, so the lockfile
// has a single entry and never needs a network refresh.
//
// Also exports readVersion / bumpVersion / setVersion for scripts/release.mjs.

import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

// Repo root is one level up from this script's scripts/ directory.
const root = join(dirname(fileURLToPath(import.meta.url)), "..");

// Each target: the file and a regex whose group 1 captures the key/prefix, so
// `$1"<version>"` swaps only the value.
const TARGETS = [
  // The (?m)^ anchor hits the [package] version, never a dependency line
  // (minibucket has none, but the anchor keeps this correct regardless).
  { file: "Cargo.toml", re: /^(version = )"[^"]*"/m },
  // minibucket's own entry in the lockfile.
  { file: "Cargo.lock", re: /(name = "minibucket"\r?\nversion = )"[^"]*"/ },
];

/** Read the current version from Cargo.toml's [package] section. */
export function readVersion() {
  const toml = readFileSync(join(root, "Cargo.toml"), "utf8");
  const m = /^version = "([^"]*)"/m.exec(toml);
  if (!m) throw new Error("could not read [package] version from Cargo.toml");
  return m[1];
}

/** Bump a semver string by "patch" | "minor" | "major". */
export function bumpVersion(current, kind) {
  const m = /^(\d+)\.(\d+)\.(\d+)$/.exec(current);
  if (!m) throw new Error(`current version is not plain semver: ${current}`);
  let [major, minor, patch] = m.slice(1).map(Number);
  if (kind === "major") [major, minor, patch] = [major + 1, 0, 0];
  else if (kind === "minor") [minor, patch] = [minor + 1, 0];
  else if (kind === "patch") patch++;
  else throw new Error(`unknown bump "${kind}" (use patch|minor|major)`);
  return `${major}.${minor}.${patch}`;
}

/** Write `version` into every manifest. Throws if any pattern fails to match. */
export function setVersion(version) {
  if (!/^\d+\.\d+\.\d+/.test(version))
    throw new Error(`invalid version "${version}" (expected x.y.z)`);
  for (const { file, re } of TARGETS) {
    const path = join(root, file);
    const before = readFileSync(path, "utf8");
    const after = before.replace(re, `$1"${version}"`);
    if (after === before)
      throw new Error(`no version match in ${file} — pattern may be stale`);
    writeFileSync(path, after);
    console.log(`  ${file}`);
  }
  console.log(`Stamped version ${version}.`);
}

// Resolve a CLI argument to a concrete version: a bump keyword or an explicit x.y.z.
export function resolveVersion(arg) {
  return ["patch", "minor", "major"].includes(arg)
    ? bumpVersion(readVersion(), arg)
    : arg;
}

// CLI entry point (only when run directly, not when imported).
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const arg = process.argv[2];
  if (!arg) {
    console.error("usage: node scripts/set-version.mjs <patch|minor|major|x.y.z>");
    process.exit(1);
  }
  setVersion(resolveVersion(arg));
}
