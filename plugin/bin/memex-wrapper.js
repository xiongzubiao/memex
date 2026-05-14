#!/usr/bin/env node
const { execFileSync, spawnSync } = require("child_process");
const path = require("path");
const fs = require("fs");
const os = require("os");

// Off by default — the `which` fork added 5-15ms to every invocation just to
// service a niche case. Set MEMEX_DELEGATE_TO_PATH=1 when you have both a
// global `npm install -g` and the Claude Code marketplace plugin and want
// both wrappers to converge on whichever install owns PATH (single platform
// binary, single ~/.memex/ state).
function memexOnPathIfNotSelf() {
  if (process.env.MEMEX_DELEGATE_TO_PATH !== "1") return null;
  const which = spawnSync("which", ["memex"], { encoding: "utf8" });
  if (which.status !== 0) return null;
  const candidate = which.stdout.trim();
  if (!candidate) return null;
  try {
    const me = fs.realpathSync(__filename);
    const them = fs.realpathSync(candidate);
    if (them === me) return null;
  } catch { return null; }
  return candidate;
}

function resolvePlatformBinary() {
  if (os.platform() === "win32") {
    console.error(
      "memex does not yet support Windows. The daemon requires Unix sockets and flock.\n" +
      "Workaround: run memex inside WSL2.\n" +
      "Track Windows support at https://github.com/xiongzubiao/memex (private; auth required)."
    );
    process.exit(1);
  }
  const pkg = `@xiongzubiao/memex-${os.platform()}-${os.arch()}`;

  // 1. Standard node resolution — works when this wrapper is invoked via
  //    npm's bin symlink, which lives in a tree containing node_modules.
  try {
    const pkgJsonPath = require.resolve(`${pkg}/package.json`);
    return path.join(path.dirname(pkgJsonPath), "bin", "memex");
  } catch { /* fall through */ }

  // 2. Claude Code marketplace install: the wrapper is copied into
  //    ~/.claude/plugins/cache/<owner>/<plugin>/<ver>/bin/, while npm
  //    drops the platform package at ~/.claude/plugins/npm-cache/node_modules/.
  //    Sibling trees — require.resolve cannot bridge them. Probe directly.
  const cc = path.join(os.homedir(), ".claude", "plugins", "npm-cache",
                       "node_modules", pkg, "bin", "memex");
  if (fs.existsSync(cc)) return cc;

  console.error(
    `memex does not ship a binary for ${os.platform()}-${os.arch()}.\n` +
    `Supported platforms: darwin-arm64, darwin-x64, linux-x64, linux-arm64.\n` +
    `To build from source: clone https://github.com/xiongzubiao/memex (private; auth required) ` +
    `and run cargo build --release -p memex-cli.`
  );
  process.exit(1);
}

const target = memexOnPathIfNotSelf() ?? resolvePlatformBinary();
try {
  execFileSync(target, process.argv.slice(2), { stdio: "inherit" });
} catch (e) {
  if (e.signal) process.exit(128);
  if (typeof e.status === "number") process.exit(e.status);
  console.error(`memex binary failed to launch: ${e.message}`);
  process.exit(1);
}
