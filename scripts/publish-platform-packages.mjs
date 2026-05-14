// Stage and publish the 4 platform packages from CI in parallel.
// Reads version from plugin/package.json. Idempotent: skips already-published.
import { mkdirSync, writeFileSync, copyFileSync, chmodSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { distTagFor } from "./release-utils.mjs";

const exec = promisify(execFile);

const VERSION = JSON.parse(readFileSync("plugin/package.json", "utf8")).version;
const TAG = distTagFor(VERSION);

const platforms = [
  { name: "darwin-arm64", os: "darwin", cpu: "arm64", artifact: "memex-darwin-arm64" },
  { name: "darwin-x64",   os: "darwin", cpu: "x64",   artifact: "memex-darwin-x64"   },
  { name: "linux-x64",    os: "linux",  cpu: "x64",   artifact: "memex-linux-x64"    },
  { name: "linux-arm64",  os: "linux",  cpu: "arm64", artifact: "memex-linux-arm64"  },
];

async function alreadyPublished(pkg, version) {
  try {
    await exec("npm", ["view", `${pkg}@${version}`, "version"]);
    return true;
  } catch {
    return false;
  }
}

async function publishPlatform(p) {
  const pkgName = `@xiongzubiao/memex-${p.name}`;
  if (await alreadyPublished(pkgName, VERSION)) {
    console.log(`Skipping ${pkgName}@${VERSION} — already on npm.`);
    return;
  }

  const stagingDir = join("npm-staging", p.name);
  const binDir = join(stagingDir, "bin");
  mkdirSync(binDir, { recursive: true });

  const src = join(p.artifact, p.artifact);
  const dst = join(binDir, "memex");
  copyFileSync(src, dst);
  chmodSync(dst, 0o755);

  writeFileSync(
    join(stagingDir, "package.json"),
    JSON.stringify({
      name:            pkgName,
      version:         VERSION,
      description:     `memex CLI binary for ${p.os}-${p.cpu}`,
      author:          "Zubiao Xiong",
      os:              [p.os],
      cpu:             [p.cpu],
      preferUnplugged: true,
      files:           ["bin/"],
      repository:      { type: "git", url: "git+https://github.com/xiongzubiao/memex.git" },
    }, null, 2) + "\n",
  );

  console.log(`Publishing ${pkgName}@${VERSION}${TAG ? ` (tag: ${TAG})` : ""}…`);
  const args = ["publish", "--access", "public"];
  if (TAG) args.push("--tag", TAG);
  const { stdout, stderr } = await exec("npm", args, { cwd: stagingDir });
  if (stdout.trim()) process.stdout.write(`[${p.name}] ${stdout}`);
  if (stderr.trim()) process.stderr.write(`[${p.name}] ${stderr}`);
}

const results = await Promise.allSettled(platforms.map(publishPlatform));
const failures = results
  .map((r, i) => ({ name: platforms[i].name, r }))
  .filter(({ r }) => r.status === "rejected");
if (failures.length > 0) {
  for (const { name, r } of failures) {
    console.error(`${name}: ${r.reason?.stderr || r.reason}`);
  }
  process.exit(1);
}
