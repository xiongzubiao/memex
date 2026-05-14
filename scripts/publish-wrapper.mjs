// Publish the wrapper package after the 4 platform packages are up.
// Mirrors scripts/publish-platform-packages.mjs in shape.
//
// Defensive optionalDependencies rewrite: scripts/release.sh already bumps
// these on commit, but we re-apply here so a manual tag-and-push without
// release.sh still publishes a wrapper with correct deps.
import { readFileSync, writeFileSync } from "node:fs";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { distTagFor } from "./release-utils.mjs";

const exec = promisify(execFile);

const PKG_JSON = "plugin/package.json";
const pkg = JSON.parse(readFileSync(PKG_JSON, "utf8"));
const VERSION = pkg.version;
const TAG = distTagFor(VERSION);

let rewrote = false;
for (const k of Object.keys(pkg.optionalDependencies || {})) {
  if (pkg.optionalDependencies[k] !== VERSION) {
    pkg.optionalDependencies[k] = VERSION;
    rewrote = true;
  }
}
if (rewrote) {
  writeFileSync(PKG_JSON, JSON.stringify(pkg, null, 2) + "\n");
  console.log(`Rewrote ${PKG_JSON} optionalDependencies to ${VERSION}`);
}

console.log(`Publishing @xiongzubiao/memex@${VERSION}${TAG ? ` (tag: ${TAG})` : ""}…`);
const args = ["publish", "--access", "public"];
if (TAG) args.push("--tag", TAG);
const { stdout, stderr } = await exec("npm", args, { cwd: "plugin" });
if (stdout.trim()) process.stdout.write(stdout);
if (stderr.trim()) process.stderr.write(stderr);
