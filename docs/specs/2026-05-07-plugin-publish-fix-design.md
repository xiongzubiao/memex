# memex Plugin Publish & Installation Fix — Design

## Problem

`marketplace.json` advertises plugin install via npm package `@memverge/memex`, but the package has never been published, CI has no step to publish it, the architecture as written cannot work because the source repo is private, AND the plugin's hooks format is invisible to Claude Code's marketplace plugin loader. Concretely:

- `https://registry.npmjs.org/@memverge/memex` → 404. Never published.
- `xiongzubiao/memex` is a **private** GitHub repository.
- `plugin/postinstall.js`'s `RELEASE_BASE = github.com/memverge/memex/releases/download/v${VERSION}` is wrong on two counts: wrong owner (was always `memverge/`, never matched the actual remote) AND fetching from a private repo's release assets without auth headers fails with 404.
- Three further references to `github.com/memverge/memex` in `plugin/.{claude,codex,cursor}-plugin/plugin.json` and `plugin/.codex/INSTALL.md` were always broken.
- **Hooks invisible to marketplace loader.** Claude Code reads hooks from `hooks/hooks.json` and expands `${CLAUDE_PLUGIN_ROOT}` natively. memex ships `hooks/claude-code.json` with `${MEMEX_PLUGIN_DIR}` placeholders rewritten by `postinstall.js`. Anthropic's reference plugins (e.g. `learning-output-style`) confirm this; the current memex hook setup never registers under marketplace install.

A first version of this spec proposed adding a `publish-npm` job that downloads the binary from a GitHub Release at install-time. That architecture is fundamentally incompatible with a private source repo:

1. **`npm publish --provenance` requires a public source repo** so npm can verify the package was built from publicly visible code.
2. **GitHub Release assets inherit repo visibility.** Anonymous fetches against a private-repo release URL return 404.

The user wants to keep the repo private. So the binary cannot live in a GitHub Release as the canonical install source. It needs to live in the npm tarball.

## Solution

Switch to the **esbuild / swc / biome / turbo pattern**: per-platform npm subpackages, with `os`/`cpu` constraints so npm installs only the matching one. The main package is a thin wrapper. Bundle the hooks-format fix in the same change since it's required for the marketplace install to actually do anything useful.

### Architecture

Five npm packages published per release, all under the `@xiongzubiao` scope:

| Package | What it contains | npm `os`/`cpu` |
|---------|------------------|----------------|
| `@xiongzubiao/memex-darwin-arm64` | `bin/memex` (Apple Silicon binary) | darwin / arm64 |
| `@xiongzubiao/memex-darwin-x64` | `bin/memex` (Intel Mac binary) | darwin / x64 |
| `@xiongzubiao/memex-linux-x64` | `bin/memex` (Linux x86_64 binary) | linux / x64 |
| `@xiongzubiao/memex-linux-arm64` | `bin/memex` (Linux ARM64 binary) | linux / arm64 |
| `@xiongzubiao/memex` | the wrapper: `bin/memex-wrapper.js`, hooks, skills, postinstall, marketplace metadata | (any) |

**Windows is deliberately excluded from v0.1.0.** The memex daemon currently uses Unix sockets and `flock`, neither of which works on Windows (`README.md:81` documents this). Shipping a `windows-x64` package would silently break on first daemon spawn. Add Windows in a follow-up release once the daemon abstracts the IPC primitives. The wrapper still has a code path for `os.platform() === "win32"` that prints a clear "Windows not yet supported" message and exits non-zero — better than a broken binary launching.

Linux ARM64 IS included from day one because adding it later (after a `v0.1.0` precedent without it) is harder than including it now: the build matrix gets one more entry, the platform array gets one more row.

The wrapper declares all four platform packages as `optionalDependencies`. Each platform package's `os`/`cpu` field tells npm to skip non-matching ones. Per-platform packages **do not declare `bin`** (esbuild does not; declaring it would race with the wrapper's `bin: { memex: ... }` and last-installed wins). Each also sets `"preferUnplugged": true` so Yarn Berry PnP unzips the binary into a real directory rather than a `.zip` archive (which `execFileSync` cannot exec).

End user runs `npm install -g @xiongzubiao/memex`. npm installs the wrapper plus exactly one platform package. `bin/memex-wrapper.js` resolves the platform package via `require.resolve("<pkg>/package.json")` (then joins to `bin/memex`) — using `package.json` resolution is robust under npm, pnpm, and Yarn PnP; bare `<pkg>/bin/<file>` resolution is not.

`postinstall.js` no longer fetches the binary and no longer rewrites hooks (Claude Code expands `${CLAUDE_PLUGIN_ROOT}` itself). It still:
- downloads model + tokenizer from HuggingFace (public),
- downloads ONNX Runtime from microsoft/onnxruntime GitHub Releases (public).

It also no longer runs `daemon stop` — that moves into the wrapper's first invocation (where the platform package is guaranteed installed; in postinstall it is not, due to npm's optional-dep install ordering).

### Publish flow (CI)

Tag push (`v*`) triggers `release.yml`. Job graph:

```
build (matrix × 4 platforms) ──► release (internal GH Release artifact) ──► publish-npm
                                                                             ├─ verify tag == package.json version
                                                                             ├─ for each platform package:
                                                                             │    skip if @xiongzubiao/memex-<plat>@VERSION already on npm
                                                                             │    else publish
                                                                             └─ (only after all 4 succeed) publish memex (wrapper)
```

The "skip if already published" precheck (`npm view @scope/name@version`) makes the publish step idempotent. If a previous run published 2/4 platform packages and failed on the 3rd, the next run skips the 2 already-published and continues from the 3rd. Without this, retrying after a partial failure errors with "version already exists" and blocks recovery.

If any platform-package publish fails, the wrapper publish does NOT run. Users on the failed platform stay on the previous wrapper version — they never see "Cannot find module @xiongzubiao/memex-X-Y".

`--provenance` is **not** used (private source). OIDC trusted publisher still works; configure once per package on npmjs.com.

## Goal

Ship a working `npm install -g @xiongzubiao/memex` and Claude-Code-marketplace install from a private source repo, with all binaries delivered through npm, no install-time network dependency on private GitHub release assets, hooks that actually fire on a real Claude Code session, and enough first-run UX scaffolding that a fresh installer can self-diagnose problems.

The first-run UX additions (a `memex doctor` subcommand, model-SHA verification on postinstall, an actionable empty-result message from `memex query`) are bundled here because they all gate on the publish landing — there is no useful "first install" without them. The Verdaccio CI rehearsal is bundled because it is the only end-to-end test of the optionalDeps resolution path before tagging.

## Non-goals

- **Codex / Gemini CLI hook formats.** This change fixes the Claude Code hooks path (`hooks/hooks.json` + `${CLAUDE_PLUGIN_ROOT}`). The Codex (`hooks/codex.json`) and Gemini (`hooks/gemini-cli.json`) loaders may have their own conventions; the existing files stay in place and continue to work for the manual `scripts/install-hooks.sh` path. A separate spec can address whether those agents support marketplace-style auto-loading.
- **GitHub Release as a public distribution channel.** Releases stay internal CI artifacts (private-repo by construction). End users get binaries via npm.
- **`curl … | sh` installer.** Out of scope for this change; npm-only is acceptable for v0.1.0.
- **Splitting CLI/plugin versioning.** All five packages publish at the same version, gated by the wrapper's `package.json`. Splitting can be added later.
- **Switching to release-event-triggered publish.** Tag-driven is fine.

## File changes

### `.github/workflows/release.yml` — extend matrix to 5 platforms; replace `release` with build → release → publish-npm chain

```yaml
build:
  strategy:
    matrix:
      include:
        - { os: macos-latest,    target: aarch64-apple-darwin,         binary: memex-darwin-arm64       }
        - { os: macos-13,        target: x86_64-apple-darwin,          binary: memex-darwin-x64         }
        - { os: ubuntu-latest,   target: x86_64-unknown-linux-gnu,     binary: memex-linux-x64          }
        - { os: ubuntu-latest,   target: aarch64-unknown-linux-gnu,    binary: memex-linux-arm64,
            cross: true                                                                                  }
  # … (existing build steps; for the cross: true row, install `cross` and use it instead of cargo directly,
  #    or use cargo with the gcc-aarch64-linux-gnu toolchain)
  # NOTE: windows-latest row removed — see Architecture section.

release:
  # (existing — unchanged; produces internal GH Release with all 4 binaries)

publish-npm:
  needs: release
  runs-on: ubuntu-latest
  permissions:
    contents: read
    id-token: write              # OIDC for trusted publisher
  steps:
    - uses: actions/checkout@v4
    - uses: actions/setup-node@v4
      with:
        node-version: 20
        registry-url: https://registry.npmjs.org
    - uses: actions/download-artifact@v4
    - name: Verify tag matches package.json version
      working-directory: plugin
      run: |
        TAG="${GITHUB_REF_NAME#v}"
        PKG=$(node -p "require('./package.json').version")
        if [ "$TAG" != "$PKG" ]; then
          echo "Tag $GITHUB_REF_NAME (=$TAG) does not match plugin/package.json $PKG" >&2
          exit 1
        fi
    - name: Publish platform packages (idempotent)
      run: node scripts/publish-platform-packages.mjs
    - name: Publish wrapper package
      working-directory: plugin
      run: npm publish --access public
```

The script reads `plugin/package.json.version` directly — no separate `VERSION` env var to keep in sync. Tag/file mismatch is caught earlier by the verify step.

### `scripts/publish-platform-packages.mjs` — new helper

```js
import { mkdirSync, writeFileSync, copyFileSync, chmodSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { execSync } from "node:child_process";
import { platformPackageName } from "./platform-mapping.mjs";

const VERSION = JSON.parse(readFileSync("plugin/package.json", "utf8")).version;

const platforms = [
  { name: "darwin-arm64",  os: "darwin", cpu: "arm64", artifact: "memex-darwin-arm64" },
  { name: "darwin-x64",    os: "darwin", cpu: "x64",   artifact: "memex-darwin-x64"   },
  { name: "linux-x64",     os: "linux",  cpu: "x64",   artifact: "memex-linux-x64"    },
  { name: "linux-arm64",   os: "linux",  cpu: "arm64", artifact: "memex-linux-arm64"  },
];
// All v0.1.0 binaries are Unix; ext is always empty. Windows excluded — see Architecture.

function alreadyPublished(pkg, version) {
  try {
    execSync(`npm view ${pkg}@${version} version`, { stdio: "pipe" });
    return true;
  } catch {
    return false;
  }
}

for (const p of platforms) {
  const pkgName = `@xiongzubiao/memex-${p.name}`;
  if (alreadyPublished(pkgName, VERSION)) {
    console.log(`Skipping ${pkgName}@${VERSION} — already on npm.`);
    continue;
  }

  const stagingDir = join("npm-staging", p.name);
  const binDir = join(stagingDir, "bin");
  mkdirSync(binDir, { recursive: true });

  const src = join(p.artifact, p.artifact);
  const dst = join(binDir, `memex`);
  copyFileSync(src, dst);
  chmodSync(dst, 0o755);

  writeFileSync(
    join(stagingDir, "package.json"),
    JSON.stringify({
      name:             pkgName,
      version:          VERSION,
      description:      `memex CLI binary for ${p.os}-${p.cpu}`,
      author:           "Zubiao Xiong",
      os:               [p.os],
      cpu:              [p.cpu],
      preferUnplugged:  true,
      files:            ["bin/"],
      repository:       { type: "git", url: "git+https://github.com/xiongzubiao/memex.git" },
    }, null, 2) + "\n",
  );

  console.log(`Publishing ${pkgName}@${VERSION}…`);
  execSync(`npm publish --access public`, { cwd: stagingDir, stdio: "inherit" });
}
```

Notes:
- No `bin` field on platform packages — only the wrapper exposes `memex` on PATH.
- `preferUnplugged: true` — required for Yarn Berry PnP to extract binary out of zip archive.
- `npm view` precheck makes retry-after-partial-failure work without "version already exists" errors.

### `scripts/platform-mapping.mjs` — single source of truth for platform → package name

```js
import os from "node:os";

// v0.1.0 ships only Unix platforms. The wrapper rejects win32 before reaching here.
export function platformPackageName() {
  return `@xiongzubiao/memex-${os.platform()}-${os.arch()}`;
}
```

Imported by `publish-platform-packages.mjs` for symmetry with the wrapper's resolution logic. When Windows support is added later, both files update by adding the win32→windows mapping in one place.

### `plugin/package.json` — wrapper: optionalDependencies, no postinstall binary download

```diff
 {
-  "name": "@memverge/memex",
+  "name": "@xiongzubiao/memex",
   "description": "Personal wiki storage and search engine for AI agents",
   "version": "0.1.0",
   "bin": { "memex": "./bin/memex-wrapper.js" },
   "scripts": {
     "postinstall": "node postinstall.js"
   },
+  "optionalDependencies": {
+    "@xiongzubiao/memex-darwin-arm64": "0.1.0",
+    "@xiongzubiao/memex-darwin-x64":   "0.1.0",
+    "@xiongzubiao/memex-linux-x64":    "0.1.0",
+    "@xiongzubiao/memex-linux-arm64":  "0.1.0"
+  },
   "files": [
-    "bin/",
+    "bin/memex-wrapper.js",
     "hooks/",
     "skills/",
     "postinstall.js",
     "AGENTS.md",
     "CLAUDE.md",
     "GEMINI.md",
     ".claude-plugin/",
     ".codex-plugin/",
     ".cursor-plugin/",
     ".codex/",
     "gemini-extension.json"
   ],
-  "os":  ["darwin", "linux", "win32"],
-  "cpu": ["x64", "arm64"],
   "repository": {
     "type": "git",
-    "url": "https://github.com/memverge/memex"
+    "url": "git+https://github.com/xiongzubiao/memex.git"
   },
-  "author": "MemVerge"
+  "author": "Zubiao Xiong"
 }
```

The wrapper itself is platform-agnostic; `os`/`cpu` move to the platform packages.

### `plugin/postinstall.js` — drop binary download, drop hook rewriting, drop daemon stop

The whole script collapses to model + tokenizer + ORT downloads:

```diff
- const VERSION = "0.1.0";
+ const VERSION = require("./package.json").version;
  ...
- const RELEASE_BASE = `https://github.com/memverge/memex/releases/download/v${VERSION}`;
  ...
- function getPlatformBinary() { /* … */ }
-
- function rewriteHooksFile(hooksPath, pluginDir) { /* … */ }

  async function main() {
-   const binDir = path.join(__dirname, "bin");
    const modelDir = path.join(os.homedir(), ".memex", "models");
    const libDir = path.join(os.homedir(), ".memex", "lib");
-
-   fs.mkdirSync(binDir, { recursive: true });
    fs.mkdirSync(modelDir, { recursive: true });
    fs.mkdirSync(libDir, { recursive: true });

-   // 1. Download memex binary.   ── REMOVED
-   ...
-
-   // 2. Download embedding model.
+   // 1. Download embedding model.
    ...

-   // 4. Rewrite ${MEMEX_PLUGIN_DIR} placeholders in hooks JSON.   ── REMOVED
-   //    (Claude Code expands ${CLAUDE_PLUGIN_ROOT} natively; the rewrite
-   //    was both unnecessary and invisible to the marketplace loader.)
-
-   // 5. Stop any running memex daemon.   ── REMOVED, moved to wrapper
-   //    (Platform package may not be installed when this script runs;
-   //    npm doesn't guarantee optional-deps land before parent postinstall.)
  }
```

### `plugin/bin/memex-wrapper.js` — resolve binary from platform package; opportunistic daemon stop on first run after upgrade

```js
#!/usr/bin/env node
const { execFileSync } = require("child_process");
const path = require("path");
const fs = require("fs");
const os = require("os");

function resolvePlatformBinary() {
  // Windows is intentionally not shipped in v0.1.0 — daemon uses Unix sockets + flock.
  if (os.platform() === "win32") {
    console.error(
      `memex does not yet support Windows. The daemon requires Unix sockets and flock.\n` +
      `Track Windows support at https://github.com/xiongzubiao/memex/issues (private; auth required).\n` +
      `Workaround: run memex inside WSL2.`
    );
    process.exit(1);
  }
  const plat = os.platform();
  const arch = os.arch();
  const pkg  = `@xiongzubiao/memex-${plat}-${arch}`;
  try {
    // Resolve via package.json — robust under npm, pnpm, and Yarn PnP.
    const pkgJsonPath = require.resolve(`${pkg}/package.json`);
    return path.join(path.dirname(pkgJsonPath), "bin", "memex");
  } catch {
    console.error(
      `memex does not ship a binary for ${plat}-${arch}.\n` +
      `Supported platforms: darwin-arm64, darwin-x64, linux-x64, linux-arm64.\n` +
      `To build from source: clone https://github.com/xiongzubiao/memex (private; auth required) ` +
      `and run cargo build --release -p memex-cli.`
    );
    process.exit(1);
  }
}

function warnIfPostinstallIncomplete() {
  // Postinstall writes ~/.memex/.install-status with download outcomes.
  // If the model or ORT is missing, vector search silently falls back to
  // hash embeddings — bad first impression. Warn at startup once.
  const modelPath = path.join(os.homedir(), ".memex", "models", "embedding-gemma-300m.onnx");
  if (!fs.existsSync(modelPath)) {
    console.error(
      "warning: memex embedding model not found. Search quality will be degraded. " +
      "Re-run `npm install -g @xiongzubiao/memex` or check ~/.memex/.install-status for download errors."
    );
  }
}

function maybeStopOldDaemon(binaryPath) {
  const stamp = path.join(os.homedir(), ".memex", ".last-binary");
  let prev = "";
  try { prev = fs.readFileSync(stamp, "utf8").trim(); } catch {}
  if (prev && prev !== binaryPath) {
    try { execFileSync(prev, ["daemon", "stop"], { stdio: "pipe", timeout: 5000 }); } catch {}
  }
  fs.mkdirSync(path.dirname(stamp), { recursive: true });
  fs.writeFileSync(stamp, binaryPath);
}

warnIfPostinstallIncomplete();
const binary = resolvePlatformBinary();
maybeStopOldDaemon(binary);

try {
  execFileSync(binary, process.argv.slice(2), { stdio: "inherit" });
} catch (e) {
  if (e.signal) process.exit(128);
  if (typeof e.status === "number") process.exit(e.status);
  console.error(`memex binary failed to launch: ${e.message}`);
  process.exit(1);
}
```

The `~/.memex/.last-binary` stamp lets the wrapper detect upgrades and stop the old daemon before launching the new one. This replaces the old postinstall daemon-stop and works correctly across upgrades (the previous binary's path is still on disk when this runs from the new wrapper).

### `plugin/hooks/hooks.json` — new file (copy of `claude-code.json`, with placeholder fixed)

```json
{
  "hooks": {
    "SessionEnd": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "node ${CLAUDE_PLUGIN_ROOT}/hooks/claude-code-session-end.js",
            "timeout": 10
          }
        ]
      }
    ],
    "SessionStart": [
      {
        "matcher": "startup|resume|clear",
        "hooks": [
          {
            "type": "command",
            "command": "memex daemon start"
          }
        ]
      }
    ]
  }
}
```

`hooks/claude-code.json` stays in place for the manual `scripts/install-hooks.sh` path (which still uses the `${MEMEX_PLUGIN_DIR}` rewrite — different distribution path, different conventions). Marketplace install reads the new `hooks.json`. Codex (`hooks/codex.json`) and Gemini (`hooks/gemini-cli.json`) are unchanged; whether their respective marketplace loaders pick them up is out of scope.

### `.claude-plugin/marketplace.json` — retitle marketplace + update package source

```diff
- "name": "memverge",
- "owner": { "name": "MemVerge" },
+ "name": "xiongzubiao",
+ "owner": { "name": "Zubiao Xiong" },
  "plugins": [{
    "name": "memex",
-   "source": { "source": "npm", "package": "@memverge/memex" },
+   "source": { "source": "npm", "package": "@xiongzubiao/memex" },
    "description": "Personal wiki storage and search engine for AI agents"
  }]
}
```

### `plugin/.claude-plugin/plugin.json`, `plugin/.codex-plugin/plugin.json`, `plugin/.cursor-plugin/plugin.json` — author + repository URL

```diff
- "author": { "name": "MemVerge" },
+ "author": { "name": "Zubiao Xiong" },
- "repository": "https://github.com/memverge/memex",
+ "repository": "https://github.com/xiongzubiao/memex",
```

(Apply both lines where present; some files have only `repository`.)

### `plugin/.codex/INSTALL.md` — update package name and clone URL

```diff
- npm install @memverge/memex
+ npm install @xiongzubiao/memex
…
- git clone https://github.com/memverge/memex.git
+ git clone https://github.com/xiongzubiao/memex.git
…
- npm update @memverge/memex
+ npm update @xiongzubiao/memex
```

### `cli/src/main.rs` (and adjacent) — new `memex doctor` subcommand

Add a `Doctor` variant to the `Commands` enum (around `cli/src/main.rs:34`), wire dispatch in the match block (around `main.rs:1322`), and implement `run_doctor()`:

```rust
// New command. Emits structured output: each check on its own line, severity at start.
// Exit 0 if all checks pass; exit 1 if any FAIL.
fn run_doctor() -> i32 {
    let mut had_fail = false;
    let home = std::env::var("HOME").unwrap_or_default();

    // 1. Embedding model + tokenizer.
    let model = format!("{home}/.memex/models/embedding-gemma-300m.onnx");
    print_check("model", std::path::Path::new(&model).exists(), &model, &mut had_fail);
    let tokenizer = format!("{home}/.memex/models/embedding-gemma-300m-tokenizer.json");
    print_check("tokenizer", std::path::Path::new(&tokenizer).exists(), &tokenizer, &mut had_fail);

    // 2. ONNX Runtime (system or bundled).
    let ort_locations = ["/opt/homebrew/lib/libonnxruntime.dylib",
                          "/usr/local/lib/libonnxruntime.dylib",
                          "/usr/lib/x86_64-linux-gnu/libonnxruntime.so",
                          &format!("{home}/.memex/lib/libonnxruntime.dylib"),
                          &format!("{home}/.memex/lib/libonnxruntime.so")];
    let ort_found = ort_locations.iter().find(|p| std::path::Path::new(p).exists());
    print_check("onnxruntime", ort_found.is_some(),
                &ort_found.map(|p| p.to_string()).unwrap_or_else(|| "(not found)".into()),
                &mut had_fail);

    // 3. Daemon health: try a status request to the socket.
    match crate::daemon::client::ping(std::time::Duration::from_secs(2)) {
        Ok(_)  => print_check("daemon", true,  "responsive", &mut had_fail),
        Err(e) => print_check("daemon", false, &format!("{e} (try `memex daemon start`)"), &mut had_fail),
    }

    // 4. Agent CLIs on PATH (informational only; not a fail).
    for tool in ["claude", "codex", "gemini"] {
        let on_path = std::process::Command::new("which").arg(tool).output()
            .map(|o| o.status.success()).unwrap_or(false);
        print_check(&format!("agent:{tool}"), on_path, if on_path { "on PATH" } else { "(optional)" }, &mut false);
    }

    // 5. Install-status JSON from postinstall (advisory).
    let status = format!("{home}/.memex/.install-status");
    if let Ok(s) = std::fs::read_to_string(&status) {
        println!("install-status: {}", s.trim().replace('\n', " "));
    }

    if had_fail {
        eprintln!("\nSome checks failed. Re-run `npm install -g @xiongzubiao/memex` to repair, ");
        eprintln!("or set ORT_DYLIB_PATH if you have a system ORT install in a non-standard location.");
        1
    } else {
        println!("\nmemex is ready.");
        0
    }
}

fn print_check(name: &str, ok: bool, detail: &str, had_fail: &mut bool) {
    let marker = if ok { "PASS" } else { "FAIL" };
    println!("[{marker}] {name}: {detail}");
    if !ok { *had_fail = true; }
}
```

The `daemon::client::ping` call requires a small addition to the existing daemon client — a tiny request type or reuse of the existing status request — confirm by reading `cli/src/daemon/client.rs`.

### `plugin/postinstall.js` (additional change) — SHA256 verification on model + tokenizer

After each `download(url, dest)` call for the model and tokenizer, verify the SHA256:

```js
const crypto = require("crypto");

const EXPECTED_SHA = {
  model:     "<paste model SHA256 here>",      // computed from current MODEL_URL
  tokenizer: "<paste tokenizer SHA256 here>",  // computed from current TOKENIZER_URL
};

function sha256(filepath) {
  const hash = crypto.createHash("sha256");
  hash.update(fs.readFileSync(filepath));
  return hash.digest("hex");
}

async function downloadVerified(url, dest, expected) {
  await download(url, dest);
  const got = sha256(dest);
  if (got !== expected) {
    fs.unlinkSync(dest);
    throw new Error(`SHA256 mismatch on ${url}\n  expected: ${expected}\n  got:      ${got}\nRefusing to use a model that doesn't match the version pinned in this plugin.`);
  }
}
```

Replace the `download(MODEL_URL, modelDest)` call with `downloadVerified(MODEL_URL, modelDest, EXPECTED_SHA.model)` and similarly for the tokenizer.

The expected SHAs are computed once and pinned in code:
```bash
curl -sL "<MODEL_URL>"      | sha256sum   # paste hex into EXPECTED_SHA.model
curl -sL "<TOKENIZER_URL>"  | sha256sum   # paste hex into EXPECTED_SHA.tokenizer
```

If a future model version is desired, both `MODEL_URL` and `EXPECTED_SHA.model` get bumped together. This catches HuggingFace-side artifact replacement (rare but documented) and accidental URL drift.

### `cli/src/daemon/error.rs` (and CLI render path) — actionable empty-result hint

The `RetrievalEmpty` error variant at `cli/src/daemon/error.rs:184-198` emits strings like `"retrieval_empty: no indexed content for MEMEX_ROOT"`. The fix is at the CLI side: when this error reaches the user's terminal, format with actionable next steps. Locate the call site that renders Query errors (in `cli/src/main.rs` around the `Commands::Query` dispatch) and wrap:

```rust
Err(DaemonError::RetrievalEmpty { collections }) => {
    let collections_part = if collections.is_empty() {
        String::new()
    } else {
        format!(" in collection(s): {}", collections.join(", "))
    };
    eprintln!("No indexed content yet{collections_part}.");
    eprintln!();
    eprintln!("To populate your wiki, try one of:");
    eprintln!("  • `memex backfill claude-code`  — import existing Claude Code sessions");
    eprintln!("  • `memex backfill codex`        — import existing Codex sessions");
    eprintln!("  • Open a session with the marketplace plugin installed; SessionEnd ingests automatically");
    eprintln!("  • `memex write <slug>` then paste content via stdin to add a page manually");
    1
}
```

Render path may also need updating in any tests that assert the old plain-string output (search for `retrieval_empty` test assertions and adjust).

### `.github/workflows/verdaccio-rehearsal.yml` — new CI workflow for end-to-end install rehearsal

Triggered on PRs touching `plugin/`, `scripts/`, or `release.yml`. Spins up Verdaccio in a service container, stages all 5 packages, publishes to local registry, installs the wrapper, runs `memex --help`. Linux-x64 only — Verdaccio in Mac/Windows GH runners is brittle and the optionalDeps resolution logic is the same on every platform. Catching breakage on Linux x64 catches the bug for all platforms.

```yaml
name: Verdaccio rehearsal
on:
  pull_request:
    paths:
      - 'plugin/**'
      - 'scripts/**'
      - '.github/workflows/release.yml'
      - '.github/workflows/verdaccio-rehearsal.yml'

jobs:
  rehearse:
    runs-on: ubuntu-latest
    services:
      verdaccio:
        image: verdaccio/verdaccio:5
        ports: ['4873:4873']
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with: { node-version: 20, registry-url: 'http://localhost:4873' }
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: 'x86_64-unknown-linux-gnu' }
      - name: Build linux-x64 binary
        run: |
          cargo build --release --target x86_64-unknown-linux-gnu -p memex-cli
          mkdir -p memex-linux-x64
          cp target/x86_64-unknown-linux-gnu/release/memex memex-linux-x64/memex-linux-x64
      - name: Stub remaining platform artifacts (rehearsal only — actual binaries not needed)
        run: |
          for plat in memex-darwin-arm64 memex-darwin-x64 memex-linux-arm64; do
            mkdir -p "$plat"
            echo "stub for verdaccio rehearsal" > "$plat/$plat"
          done
      - name: Auth to local Verdaccio (no real password — anonymous publish allowed by default)
        run: |
          npm config set registry http://localhost:4873
          npm config set //localhost:4873/:_authToken fake
      - name: Publish all 5 packages to Verdaccio
        run: |
          ( cd plugin && npm publish --access public --registry http://localhost:4873 )
          node scripts/publish-platform-packages.mjs
      - name: Install from local registry into a clean directory
        run: |
          mkdir /tmp/install-test && cd /tmp/install-test
          npm init -y >/dev/null
          npm install @xiongzubiao/memex --registry http://localhost:4873
          # Confirm exactly the linux-x64 platform package was selected:
          npm ls @xiongzubiao/memex
          ls node_modules/@xiongzubiao/
      - name: Smoke-test the wrapper resolves the binary
        run: |
          cd /tmp/install-test
          ./node_modules/.bin/memex --help || \
            (echo "wrapper failed — likely require.resolve regression" >&2; exit 1)
```

The CI step uses a stub binary for the non-linux-x64 platform packages because verdaccio doesn't care about content — it just needs a tarball with the right metadata. The actual platform resolution test (npm picked linux-x64, not darwin-arm64) is what matters; the stub binaries don't need to execute.

### `README.md` (root) — fix install instructions and Windows claim

Three changes needed (current `README.md:81, 119-127`):

1. **Lead with `npm install -g @xiongzubiao/memex`** as the canonical install path. Demote "clone + cd plugin && npm install" to a "Build from source" footnote (still useful for development, but not the recommended path).
2. **Update Windows note.** Current text reads "Windows is not supported (Unix sockets + flock)." Keep the technical accuracy but reframe as "Windows: not yet supported in v0.1.0 — daemon requires Unix sockets and flock. Run inside WSL2 as a workaround. Tracking issue: …"
3. **Update any remaining `@memverge/memex` references** to `@xiongzubiao/memex`. (Verify with `grep -n memverge README.md` before merging.)

### `plugin/postinstall.js` — also write `~/.memex/.install-status` JSON for marketplace error surfacing

In addition to the diff above, postinstall writes a JSON status file as the last step of `main()`:

```js
// At the end of main(), after all downloads:
fs.writeFileSync(
  path.join(os.homedir(), ".memex", ".install-status"),
  JSON.stringify({
    version: VERSION,
    timestamp: new Date().toISOString(),
    model: fs.existsSync(modelDest),
    tokenizer: fs.existsSync(tokenizerDest),
    ort: ortInfo ? fs.existsSync(path.join(libDir, ortInfo.lib)) : null,
  }, null, 2),
);
```

The wrapper's `warnIfPostinstallIncomplete()` reads this and surfaces a one-line warning if the model is missing — this is the only path Claude Code's marketplace-install user has to discover that postinstall partially failed (Claude Code runs `npm install` opaquely; postinstall stderr is captured but rarely surfaced to the user).

## Operator setup (one-time, off-repo)

1. On npmjs.com, create the scope `@xiongzubiao` (or rely on first publish). For each of the **five** packages, configure a Trusted Publisher:
   - `@xiongzubiao/memex` (wrapper)
   - `@xiongzubiao/memex-darwin-arm64`
   - `@xiongzubiao/memex-darwin-x64`
   - `@xiongzubiao/memex-linux-x64`
   - `@xiongzubiao/memex-linux-arm64`

   Each entry: repository owner `xiongzubiao`, repo name `memex`, workflow `release.yml`, environment blank.
2. No GitHub secret. OIDC handles auth via `id-token: write`.
3. `--provenance` is omitted; npm provenance requires a public source repo.

## Release procedure

A single `scripts/release.sh` wraps the release flow with pre-flight validation:

```bash
#!/usr/bin/env bash
# scripts/release.sh — bump version, tag, push. Operator-facing.
# Usage: scripts/release.sh [patch|minor|major]
set -euo pipefail

BUMP="${1:-patch}"
case "$BUMP" in patch|minor|major) ;; *) echo "usage: $0 [patch|minor|major]" >&2; exit 1;; esac

# Pre-flight: clean tree, on main, up to date.
[ -z "$(git status --porcelain)" ] || { echo "tree not clean" >&2; exit 1; }
[ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || { echo "not on main" >&2; exit 1; }
git fetch origin main >/dev/null
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "main is behind origin" >&2; exit 1; }

# Bump version (no auto-commit/tag — we do it explicitly).
( cd plugin && npm version "$BUMP" --no-git-tag-version >/dev/null )
NEW=$(node -p "require('./plugin/package.json').version")

# Confirm before pushing.
read -rp "Release v${NEW}? [y/N] " yn
[ "$yn" = "y" ] || { git checkout plugin/package.json; echo "aborted" >&2; exit 1; }

git add plugin/package.json
git commit -m "chore: release v${NEW}"
git tag "v${NEW}"
git push origin main "v${NEW}"

echo "Pushed v${NEW}. Watch CI: gh run watch"
```

Add `"release": "./scripts/release.sh"` to root `package.json` `scripts` so operators can run `npm run release -- patch` (the trailing `--` separator passes the bump arg through). The pre-flight rejects releases from a dirty tree, off-main branch, or stale local main — three common operator footguns. The confirmation prompt gives a final escape hatch.

Manual fallback (if the script can't run):

```bash
( cd plugin && npm version patch --no-git-tag-version )
git add plugin/package.json && git commit -m "chore: release v$(node -p "require('./plugin/package.json').version")"
git tag "v$(node -p "require('./plugin/package.json').version")"
git push origin main --tags
```

## Testing

**Pre-merge:**

- `cd plugin && npm publish --dry-run` — verify wrapper tarball contents.
- `cd plugin && npm pack` — confirm `bin/memex-wrapper.js`, `hooks/hooks.json`, `hooks/claude-code.json`, `skills/`, `postinstall.js`, `.claude-plugin/`, etc. all included; `bin/memex` is NOT in the wrapper tarball.
- **Verdaccio rehearsal** (the only way to test cross-platform optional-dep resolution end-to-end): run a local Verdaccio registry, publish all 5 packages to it, then `npm install -g @xiongzubiao/memex` against the local registry on a clean container per platform. Confirms only the matching platform package is downloaded.
- `actionlint .github/workflows/release.yml` — catch syntax errors before tagging.

**Post-merge first-release acceptance:**

1. Tag `v0.1.0`, push, observe `release.yml`:
   - `build` jobs (× 4) all green; the linux-arm64 row produces a binary that `file` reports as `aarch64`.
   - `release` job creates internal GH Release.
   - `publish-npm` job: tag-match guard passes, all 4 platform packages publish (or skip if already published), then wrapper publishes.
2. On clean machines (one per supported platform): `npm install -g @xiongzubiao/memex`. Confirm:
   - `npm ls @xiongzubiao/memex` shows wrapper + exactly one platform package; non-matching ones show as skipped/optional.
   - Postinstall logs: model + tokenizer + ORT downloads succeed (or skip with system ORT). No memex-binary download attempt; no hook-file rewrite.
   - `memex --help` runs successfully.
3. **Hook registration check (release blocker if it fails):** install via Claude Code marketplace; start a fresh session; confirm `memex daemon start` runs at SessionStart and `memex ingest` runs at SessionEnd.
4. Tag `v0.1.1` with a deliberate version mismatch in `package.json` (still 0.1.0) — confirm tag-match guard fails.
5. Idempotency: simulate partial publish failure by republishing `v0.1.1` without bumping; confirm the script skips already-published packages and only attempts the missing ones.

## Failure modes & rollback

### CI / publishing
- **Trusted publisher not configured for one of the 5 packages.** That package's publish fails; subsequent steps halt; wrapper does NOT publish. Fix the missing trusted-publisher config on npmjs.com; re-run the workflow (`gh workflow run release.yml --ref vX.Y.Z`). The idempotent `npm view` precheck means already-published packages are skipped on retry — no version bump needed.
- **One platform's binary build fails.** `build` job fails; `publish-npm` never runs. No partial publish. Fix the build, re-tag, push.
- **Tag-match guard catches a drift mistake.** Workflow fails before any publish. Operator fixes the version (`cd plugin && npm version <correct> --no-git-tag-version`), deletes the bad tag (`git push --delete origin vX.Y.Z && git tag -d vX.Y.Z`), commits, retags, pushes.
- **Partial publish across runs.** If 2/4 platform packages publish on run 1 and CI fails, run 2's idempotent precheck skips the 2 already-on-npm and resumes from the 3rd. The wrapper publishes only after all 4 succeed.
- **npm publish edit window expired (24h).** A bad version cannot be unpublished; ship a patch. Treat 0.1.0 as a soft-launch; verify on clean machines before announcing.

### End-user install
- **Unsupported platform (Windows in v0.1.0; FreeBSD; linux-mips; etc.).** Wrapper prints a clear "not yet supported" message naming what works and pointing to build-from-source. Better than silent failure.
- **Postinstall partially fails (model or ORT didn't download).** `~/.memex/.install-status` records what succeeded. Wrapper warns at startup if model is missing. User can re-run `npm install -g @xiongzubiao/memex` to retry, or set `ORT_DYLIB_PATH` / `MEMEX_MODEL_PATH` env vars to point at locally-cached copies.
- **Corp proxy / TLS-MITM blocks HuggingFace or microsoft/onnxruntime GitHub.** Postinstall download fails with `https.get` ECONNRESET or 403. Remediation: configure `HTTPS_PROXY` env var before `npm install`, or pre-download the model + ORT binaries to the expected paths (`~/.memex/models/embedding-gemma-300m.onnx`, `~/.memex/lib/lib<onnxruntime>.so|dylib`) and re-run install (existence guards skip re-download).
- **Disk full mid-download.** Partial file in `~/.memex/models/` or `~/.memex/lib/`. Existence guards will short-circuit on retry without re-downloading; user must `rm` the partial file. Document at the top of the README troubleshooting section.
- **ORT extraction fails (`tar` missing on bare container, PowerShell blocked on locked-down Windows).** N/A for Windows in v0.1.0; for Linux containers without `tar`, install via `apk add tar` / `apt install tar` and re-run.
- **Marketplace install: postinstall stderr never surfaces to user.** This is the gap `~/.memex/.install-status` + wrapper startup warning addresses. If a Claude Code marketplace user hits a postinstall failure, the next time they invoke any `memex` command they see the warning line.

### Daemon lifecycle
- **`require.resolve` works, but old daemon (from previous version) keeps running.** Wrapper's `~/.memex/.last-binary` stamp triggers `daemon stop` on the previous binary before launching the new one. If the stamp is stale or missing, the old daemon eventually gets supplanted on next session-start hook fire.
- **`require.resolve` fails at runtime on an exotic platform.** Wrapper exits with an actionable error message including build-from-source instructions.

## Open flags / out-of-scope notes

- **Codex / Gemini CLI marketplace hook auto-loading.** This change fixes Claude Code's hooks path. Codex and Gemini may or may not respect their own `hooks/codex.json` / `hooks/gemini-cli.json` files at install time. Separate spec; existing manual `scripts/install-hooks.sh` path still works.
- **macOS x86_64 sunset.** `release.yml` builds darwin-x64 on `macos-13` (last GH-hosted x86_64 macOS runner). Independent of this change.
- **Future MemVerge republish.** If `@memverge/memex` becomes desired later, it's 5 package renames + 5 new trusted-publisher configs. Use `npm deprecate '@xiongzubiao/memex@*' "Moved to @memverge/memex"` to push existing users to the new scope.
- **Windows support.** Add in a follow-up release once the daemon abstracts Unix sockets and `flock` (e.g., named pipes + LockFileEx). At that point: add a `windows-x64` row to the build matrix, a 6th platform package + trusted publisher, restore the `os.platform() === "win32"` mapping in `scripts/platform-mapping.mjs`, and remove the win32-rejection branch in the wrapper.
- **Repo visibility flip.** If the repo is later made public, `--provenance` can be added to all publish commands for free supply-chain attestation. No code restructure.
- **Verdaccio integration test in CI** (vs. local pre-merge). Could be added later as a workflow that runs on PRs touching `plugin/` or `release.yml`.
- **Codex / Gemini CLI marketplace hook auto-loading.** Confirmed (`codex plugin marketplace add --help`) that Codex has a marketplace, but it's git-based (`owner/repo[@ref]`), not npm-based, so `.claude-plugin/marketplace.json` does not apply. Whether Codex auto-loads `hooks/codex.json` from an installed plugin and whether it has a `${CODEX_PLUGIN_ROOT}`-equivalent placeholder are unknown — research-blocked. Gemini's marketplace install path (if any) is also unknown; `gemini-extension.json` is just metadata. Tracked as a separate research-then-implement spec.
