#!/usr/bin/env node
// set-versions.js — write the release version into whichever manifests this
// project ships. This is scaffolded once from the example and then belongs
// to the project. `releaser init` and `releaser upgrade` look for files that
// carry a version and activate the matching calls at the bottom; the other
// calls stay there, commented out, for reference.
//
// Run from .releaserc.json's prepare step:  node build_tools/set-versions.js <version>
//
// package.json takes the semantic-release version verbatim (npm speaks semver).
//
// manifest.json cannot: a WebExtension "version" only accepts 1-4 dot-separated
// integers (0-65535), so the pre-release form X.Y.Z-pre.N has its trailing
// counter folded into a fourth component instead.
//
//   1.2.0          ->  1.2.0
//   1.2.0-pre.4    ->  1.2.0.4
//   1.2.0-pre.4+ci ->  1.2.0.4   (build metadata dropped)
//
// Note that 1.2.0.4 sorts *after* 1.2.0 for the browser, whereas semver puts
// the pre-release first. That is unavoidable within the manifest format and
// harmless here: alpha builds are self-distributed, never upgraded in place
// from a stable release.
//
// pyproject.toml (PEP 440) takes a differently-shaped pre-release: semver's
// "-pre.N" separator and identifier have no PEP 440 equivalent, so the
// identifier is mapped onto the nearest PEP 440 spelling and the separating
// dot is dropped.
//
//   1.2.0          ->  1.2.0
//   1.2.0-pre.4    ->  1.2.0rc4    ("pre" is PEP 440's own alias for "rc")
//   1.2.0-rc.4     ->  1.2.0rc4
//   1.2.0-alpha.4  ->  1.2.0a4
//   1.2.0-beta.4   ->  1.2.0b4
//   1.2.0-dev.4    ->  1.2.0.dev4
//   1.2.0-pre.4+ci ->  1.2.0rc4    (build metadata dropped)
//
// If a project's pre-release identifier doesn't need to reach pyproject.toml
// at all, drop the setPyprojectVersion() call instead of trying to widen
// toPep440Version() to fit — it is deliberately narrow.
//
// Cargo.toml takes the semantic-release version verbatim, like package.json:
// Cargo speaks semver, pre-release identifiers included. Cargo.lock records the
// root package's version too, and `cargo build --locked` fails if it is stale.
// A workspace that keeps the version in [workspace.package] passes
// { table: "workspace.package" } to setTomlVersion() instead.
//
// A Helm chart's Chart.yaml carries two versions. "version" is the chart's own
// and Helm requires it to be SemVer 2, so it takes the semantic-release version
// verbatim. "appVersion" names the application the chart deploys; for a chart
// that ships beside its application it is the same release, so it is set too
// unless { appVersion: false } is passed. It is written quoted, because an
// unquoted 1.2 would be read back as a YAML float.
//
// A Makefile takes the semantic-release version verbatim into a literal
// assignment such as `VERSION := 1.2.0`. A Makefile that already derives its
// version (from VERSION.txt or `git describe`) needs no call at all.
//
// CMake's project(... VERSION ...) only accepts 1-4 dot-separated integers, so
// it gets the same folding as manifest.json: 1.2.0-pre.4 -> 1.2.0.4.

const fs = require("fs");
const path = require("path");

const ROOT = path.join(__dirname, "..");

// X.Y.Z[-label.N] -> X.Y.Z[.N]: the pre-release counter becomes a fourth
// numeric component, for formats that know nothing else.
function toNumericVersion(semver) {
  const [core, prerelease] = semver.replace(/\+.*$/, "").split("-", 2);

  if (!/^\d+\.\d+\.\d+$/.test(core)) {
    throw new Error(`cannot parse '${semver}' as X.Y.Z[-prerelease]`);
  }
  if (!prerelease) return core;

  // Take the counter off the end of the identifier ("pre.4", "rc4", "beta.4").
  const counter = prerelease.match(/(\d+)$/);
  if (!counter) {
    throw new Error(`pre-release '${prerelease}' carries no numeric counter`);
  }

  return `${core}.${counter[1]}`;
}

function toManifestVersion(semver) {
  const version = toNumericVersion(semver);
  if (version.split(".").some((n) => Number(n) > 65535)) {
    throw new Error(`'${version}' exceeds the manifest limit of 65535 per part`);
  }
  return version;
}

const PEP440_SEGMENTS = { pre: "rc", rc: "rc", c: "rc", alpha: "a", a: "a", beta: "b", b: "b" };

function toPep440Version(semver) {
  const [core, prerelease] = semver.replace(/\+.*$/, "").split("-", 2);

  if (!/^\d+\.\d+\.\d+$/.test(core)) {
    throw new Error(`cannot parse '${semver}' as X.Y.Z[-prerelease]`);
  }
  if (!prerelease) return core;

  const match = prerelease.match(/^([a-zA-Z]+)\.?(\d+)$/);
  if (!match) {
    throw new Error(`pre-release '${prerelease}' has no PEP 440 equivalent`);
  }
  const [, label, counter] = match;

  if (label === "dev") return `${core}.dev${counter}`;

  const segment = PEP440_SEGMENTS[label.toLowerCase()];
  if (!segment) {
    throw new Error(`pre-release label '${label}' has no PEP 440 equivalent`);
  }
  return `${core}${segment}${counter}`;
}

// Rewrite one key in a JSON file. JSON.stringify preserves key order, so the
// diff stays down to the single line that changed.
function setVersion(file, version) {
  const target = path.join(ROOT, file);
  const raw = fs.readFileSync(target, "utf8");
  const json = JSON.parse(raw);

  json.version = version;
  fs.writeFileSync(target, JSON.stringify(json, null, 2) + (raw.endsWith("\n") ? "\n" : ""));
  console.log(`${file}: version -> ${version}`);
}

// Rewrite `version = "..."` inside a TOML file's [project] table, leaving
// everything else (comments, formatting, other tables with their own
// "version" key) untouched. A real TOML parser would round-trip fine too,
// but pulling one in is unnecessary for a single scalar and this keeps the
// diff to the one line that changed, like setVersion() above.
function setTomlVersion(file, version, { table = "project" } = {}) {
  const target = path.join(ROOT, file);
  const raw = fs.readFileSync(target, "utf8");
  const lines = raw.split("\n");

  const tableHeader = new RegExp(`^\\s*\\[${table.replace(/\./g, "\\.")}\\]\\s*$`);
  const anyHeader = /^\s*\[/;
  const versionLine = /^(\s*version\s*=\s*)(["'])(?:(?!\2).)*\2(.*)$/;

  const start = lines.findIndex((l) => tableHeader.test(l));
  if (start === -1) {
    throw new Error(`no [${table}] table in ${file}`);
  }
  let end = lines.length;
  for (let i = start + 1; i < lines.length; i++) {
    if (anyHeader.test(lines[i])) {
      end = i;
      break;
    }
  }

  for (let i = start + 1; i < end; i++) {
    const m = lines[i].match(versionLine);
    if (!m) continue;
    lines[i] = `${m[1]}${m[2]}${version}${m[2]}${m[3]}`;
    fs.writeFileSync(target, lines.join("\n"));
    console.log(`${file}: version -> ${version}`);
    return;
  }
  throw new Error(`[${table}] in ${file} has no literal "version" key (dynamic?)`);
}

// pyproject.toml's own version, in [project].
function setPyprojectVersion(version) {
  setTomlVersion("pyproject.toml", version);
}

// A lockfile records the root package's version too (it's just another entry
// in its [[package]] list), and the package manager will flag it as stale if
// it's left at the old value after the manifest moves on. uv.lock and
// Cargo.lock share this layout.
function setLockVersion(file, version, { packageName } = {}) {
  const target = path.join(ROOT, file);
  const raw = fs.readFileSync(target, "utf8");
  const lines = raw.split("\n");

  const nameLine = new RegExp(`^name = "${packageName}"$`);
  const idx = lines.findIndex((l) => nameLine.test(l));
  if (idx === -1) {
    throw new Error(`no package "${packageName}" in ${file}`);
  }
  const versionLine = /^(version = )"[^"]*"(.*)$/;
  for (let i = idx + 1; i < lines.length && !/^\[\[package\]\]$/.test(lines[i]); i++) {
    const m = lines[i].match(versionLine);
    if (!m) continue;
    lines[i] = `${m[1]}"${version}"${m[2]}`;
    fs.writeFileSync(target, lines.join("\n"));
    console.log(`${file}: ${packageName} version -> ${version}`);
    return;
  }
  throw new Error(`package "${packageName}" in ${file} has no version line`);
}

// `uv sync`/`uv lock --check` flag a stale uv.lock.
function setUvLockVersion(version, opts) {
  setLockVersion("uv.lock", version, opts);
}

// Cargo.toml's own version, in [package].
function setCargoVersion(version) {
  setTomlVersion("Cargo.toml", version, { table: "package" });
}

function setCargoLockVersion(version, opts) {
  setLockVersion("Cargo.lock", version, opts);
}

// Rewrite the top-level `version:` (and `appVersion:`) keys of a Chart.yaml
// line by line, like setTomlVersion(). Only unindented keys are touched, so a
// "version" under dependencies: is left alone. Charts usually live in a
// subdirectory (charts/<name>/Chart.yaml), hence the path argument.
function setHelmChartVersion(file, version, { appVersion = true } = {}) {
  const target = path.join(ROOT, file);
  const lines = fs.readFileSync(target, "utf8").split("\n");

  // An existing quote style is kept; `fallbackQuote` applies to a bare value.
  const setKey = (key, value, fallbackQuote) => {
    const keyLine = new RegExp(`^(${key}:\\s*)(["']?)[^"'#\\s]*\\2(\\s*(?:#.*)?)$`);
    const idx = lines.findIndex((l) => keyLine.test(l));
    if (idx === -1) {
      throw new Error(`${file} has no top-level ${key}`);
    }
    const [, prefix, quote, rest] = lines[idx].match(keyLine);
    const q = quote || fallbackQuote;
    lines[idx] = `${prefix}${q}${value}${q}${rest}`;
    console.log(`${file}: ${key} -> ${value}`);
  };

  setKey("version", version, "");
  if (appVersion) setKey("appVersion", version, '"');
  fs.writeFileSync(target, lines.join("\n"));
}

// Rewrite a literal top-level make assignment (`VERSION = 1.2.0`, `:=`, `?=`,
// `::=`). A leading "v" on the old value is kept. Recipe lines start with a tab
// and are never matched.
function setMakeVersion(file, version, { variable = "VERSION" } = {}) {
  const target = path.join(ROOT, file);
  const lines = fs.readFileSync(target, "utf8").split("\n");

  const assignment = new RegExp(`^(${variable}\\s*(?::{1,3}=|\\?=|=)\\s*)(v?)\\d[^\\s#]*(.*)$`);
  const idx = lines.findIndex((l) => assignment.test(l));
  if (idx === -1) {
    throw new Error(`${file} has no literal ${variable} assignment`);
  }
  const [, prefix, v, rest] = lines[idx].match(assignment);
  lines[idx] = `${prefix}${v}${version}${rest}`;
  fs.writeFileSync(target, lines.join("\n"));
  console.log(`${file}: ${variable} -> ${version}`);
}

// Rewrite the VERSION argument of the first project() call in a
// CMakeLists.txt. Pass it toNumericVersion(), CMake accepts nothing else.
function setCMakeVersion(file, version) {
  const target = path.join(ROOT, file);
  const raw = fs.readFileSync(target, "utf8");

  const project = /(\bproject\s*\([^)]*?\bVERSION\s+)("?)[0-9][0-9.]*\2/i;
  if (!project.test(raw)) {
    throw new Error(`${file} has no project(... VERSION x.y.z)`);
  }
  fs.writeFileSync(target, raw.replace(project, `$1$2${version}$2`));
  console.log(`${file}: project VERSION -> ${version}`);
}

const semver = process.argv[2];
if (!semver) {
  console.error("usage: set-versions.js <version>");
  process.exit(1);
}

try {
  // Cargo.toml: Rust crate tmux-ssh-agent 0.1.0
  setCargoVersion(semver);
  setCargoLockVersion(semver, { packageName: "tmux-ssh-agent" });

  // Everything below is the catalogue, commented out: uncomment what this
  // project needs and delete the rest.
  //
  // Node packages and WebExtensions:
  // setVersion("package.json", semver);
  // setVersion("manifest.json", toManifestVersion(semver));
  //
  // Python projects, with the package's own name (the "name" under this
  // project's [project] table in pyproject.toml). A Poetry project passes
  // { table: "tool.poetry" } to setTomlVersion("pyproject.toml", ...) instead.
  // setPyprojectVersion(toPep440Version(semver));
  // setUvLockVersion(toPep440Version(semver), { packageName: "CHANGEME" });
  //
  // Rust projects, with the crate's name from [package] in Cargo.toml.
  // setCargoVersion(semver);
  // setCargoLockVersion(semver, { packageName: "CHANGEME" });
  //
  // Helm charts: one call per chart, with the path to its Chart.yaml. Add
  // { appVersion: false } if the chart's appVersion tracks something else.
  // setHelmChartVersion("charts/CHANGEME/Chart.yaml", semver);
  //
  // Makefiles and CMake projects:
  // setMakeVersion("Makefile", semver, { variable: "VERSION" });
  // setCMakeVersion("CMakeLists.txt", toNumericVersion(semver));
} catch (err) {
  console.error(`set-versions: ${err.message}`);
  process.exit(1);
}
