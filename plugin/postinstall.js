#!/usr/bin/env node
const https = require("https");
const fs = require("fs");
const path = require("path");
const os = require("os");
const crypto = require("crypto");
const { execSync } = require("child_process");

const VERSION = require("./package.json").version;
// ort 2.0.0-rc.12 with api-23 targets ORT 1.23.x; 1.24 dropped x86_64 macOS.
const ORT_VERSION = "1.23.2";
const MODEL_URL = "https://huggingface.co/LeePark/gemma-embedding-300M-onnx-int8/resolve/main/model_int8.onnx";
const TOKENIZER_URL = "https://huggingface.co/onnx-community/embeddinggemma-300m-ONNX/resolve/main/tokenizer.json";
const ORT_RELEASE_BASE = `https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}`;

// Pinned SHA256 of the model + tokenizer at the URLs above. Bump together with the
// URLs when changing model versions; mismatch causes the file to be deleted and the
// install to fail. Set to placeholder values until the v0.1.0 publish — when unset,
// verification is skipped with a warning.
const EXPECTED_SHA = {
  model:     "daa9fb37fa1d6f793bf100c7bb42be7839592d380b4f2188a8dadb1d88c6222e",
  tokenizer: "4dda02faaf32bc91031dc8c88457ac272b00c1016cc679757d1c441b248b9c47",
};

function getOrtArchive() {
  const key = `${os.platform()}-${os.arch()}`;
  const archives = {
    "darwin-arm64": { file: `onnxruntime-osx-arm64-${ORT_VERSION}.tgz`,     lib: "libonnxruntime.dylib" },
    "darwin-x64":   { file: `onnxruntime-osx-x86_64-${ORT_VERSION}.tgz`,    lib: "libonnxruntime.dylib" },
    "linux-x64":    { file: `onnxruntime-linux-x64-${ORT_VERSION}.tgz`,     lib: "libonnxruntime.so"    },
    "linux-arm64":  { file: `onnxruntime-linux-aarch64-${ORT_VERSION}.tgz`, lib: "libonnxruntime.so"    },
  };
  return archives[key] || null;
}

function download(url, dest) {
  return new Promise((resolve, reject) => {
    https.get(url, (res) => {
      if (res.statusCode === 301 || res.statusCode === 302) {
        res.resume();
        return download(res.headers.location, dest).then(resolve).catch(reject);
      }
      if (res.statusCode !== 200) {
        res.resume();
        return reject(new Error(`Download failed: ${res.statusCode} from ${url}`));
      }
      const file = fs.createWriteStream(dest);
      res.pipe(file);
      file.on("finish", () => { file.close(); resolve(); });
      file.on("error", reject);
    }).on("error", reject);
  });
}

// Stream the hash so we don't load the whole file (model is ~329MB) into RAM.
function sha256(filepath) {
  return new Promise((resolve, reject) => {
    const hash = crypto.createHash("sha256");
    fs.createReadStream(filepath)
      .on("data", (chunk) => hash.update(chunk))
      .on("end", () => resolve(hash.digest("hex")))
      .on("error", reject);
  });
}

function shaIsPinned(value) {
  return typeof value === "string" && value.length === 64 && /^[0-9a-f]+$/.test(value);
}

async function downloadVerified(url, dest, expected, label) {
  await download(url, dest);
  if (shaIsPinned(expected)) {
    const got = await sha256(dest);
    if (got !== expected) {
      fs.unlinkSync(dest);
      throw new Error(
        `SHA256 mismatch on ${label}\n  url:      ${url}\n  expected: ${expected}\n  got:      ${got}\n` +
        `Refusing to use a download that does not match the version pinned in this plugin.`
      );
    }
  } else {
    console.log(`(${label}: SHA256 verification skipped — no pinned digest)`);
  }
}

/// Download (or re-download if cached + checksum mismatch) one pinned asset.
async function ensureAsset({ dest, url, expectedSha, label, sizeHint }) {
  if (!fs.existsSync(dest)) {
    console.log(`Downloading ${label}${sizeHint ? ` (${sizeHint})` : ""}...`);
    await downloadVerified(url, dest, expectedSha, label);
    console.log(`${label} downloaded.`);
    return;
  }
  if (shaIsPinned(expectedSha) && (await sha256(dest)) !== expectedSha) {
    console.log(`Cached ${label} checksum mismatch; re-downloading...`);
    fs.unlinkSync(dest);
    await downloadVerified(url, dest, expectedSha, label);
  }
}

function copyLibFiles(dir, dest, prefix) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) copyLibFiles(full, dest, prefix);
    else if (entry.name.startsWith(prefix)) fs.copyFileSync(full, path.join(dest, entry.name));
  }
}

function findSystemOrt(libName) {
  if (process.env.ORT_DYLIB_PATH && fs.existsSync(process.env.ORT_DYLIB_PATH)) {
    return process.env.ORT_DYLIB_PATH;
  }
  const candidates = ["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib", "/usr/lib/x86_64-linux-gnu"];
  for (const dir of candidates) {
    const p = path.join(dir, libName);
    if (fs.existsSync(p)) return p;
  }
  return null;
}

async function main() {
  const status = { version: VERSION, timestamp: new Date().toISOString(), model: false, tokenizer: false, ort: null };
  const memexHome = path.join(os.homedir(), ".memex");
  const modelDir = path.join(memexHome, "models");
  const libDir = path.join(memexHome, "lib");

  fs.mkdirSync(memexHome, { recursive: true });
  fs.mkdirSync(modelDir, { recursive: true });
  fs.mkdirSync(libDir, { recursive: true });

  const modelDest = path.join(modelDir, "embedding-gemma-300m.onnx");
  await ensureAsset({
    dest: modelDest,
    url: MODEL_URL,
    expectedSha: EXPECTED_SHA.model,
    label: "embedding model",
    sizeHint: "~329MB",
  });
  status.model = fs.existsSync(modelDest);

  const tokenizerDest = path.join(modelDir, "embedding-gemma-300m-tokenizer.json");
  await ensureAsset({
    dest: tokenizerDest,
    url: TOKENIZER_URL,
    expectedSha: EXPECTED_SHA.tokenizer,
    label: "tokenizer",
    sizeHint: "~20MB",
  });
  status.tokenizer = fs.existsSync(tokenizerDest);

  // ONNX Runtime.
  const ortInfo = getOrtArchive();
  if (ortInfo) {
    const libDest = path.join(libDir, ortInfo.lib);
    const systemOrt = findSystemOrt(ortInfo.lib);
    if (systemOrt) {
      console.log(`ONNX Runtime found at ${systemOrt} — skipping download.`);
      status.ort = true;
    } else if (!fs.existsSync(libDest)) {
      const archiveDest = path.join(libDir, ortInfo.file);
      console.log(`Downloading ONNX Runtime v${ORT_VERSION}...`);
      await download(`${ORT_RELEASE_BASE}/${ortInfo.file}`, archiveDest);
      console.log("Extracting ONNX Runtime...");
      const tmpDir = path.join(libDir, "_ort_tmp");
      fs.mkdirSync(tmpDir, { recursive: true });
      // --no-same-owner: don't preserve UID/GID from the tarball. The ORT
      // archive's tar headers carry the build-machine's UID (cloudtest, UID
      // 1000) which would otherwise stick when extracting as root, leaving
      // files owned by whichever local user happens to share UID 1000.
      execSync(`tar xzf "${archiveDest}" -C "${tmpDir}" --no-same-owner`, { stdio: "pipe" });
      copyLibFiles(tmpDir, libDir, ortInfo.lib);
      fs.rmSync(tmpDir, { recursive: true, force: true });
      fs.rmSync(archiveDest, { force: true });
      console.log("ONNX Runtime ready.");
      status.ort = fs.existsSync(libDest);
    } else {
      status.ort = true;
    }
  }

  // Write install status for the wrapper to surface postinstall failures at startup.
  fs.writeFileSync(path.join(memexHome, ".install-status"), JSON.stringify(status, null, 2));

  // Daemon-stop on upgrade: if the previously-installed binary lives at a
  // different path AND is still on disk, stop its daemon before the user's
  // next invocation hits the new binary. Then update the stamp.
  // Used to live in the wrapper (every-invocation cost); now once at install.
  try {
    const platformPkg = `@xiongzubiao/memex-${os.platform()}-${os.arch()}`;
    const newBinaryPath = path.join(
      path.dirname(require.resolve(`${platformPkg}/package.json`)),
      "bin", "memex"
    );
    const stamp = path.join(memexHome, ".last-binary");
    let prev = "";
    try { prev = fs.readFileSync(stamp, "utf8").trim(); } catch {}
    if (prev && prev !== newBinaryPath && fs.existsSync(prev)) {
      try {
        execSync(`${JSON.stringify(prev)} daemon stop`, { stdio: "pipe", timeout: 5000 });
      } catch { /* old daemon wasn't running, or refused — ignore */ }
    }
    fs.writeFileSync(stamp, newBinaryPath);
  } catch {
    // Platform package not resolvable (unsupported arch). Leave stamp untouched
    // so a subsequent install on a supported arch picks up cleanly.
  }

  // Global install (`npm install -g`) → also register hooks with detected
  // agents so the user gets the full "one command" install. Skip otherwise
  // (the marketplace path runs npm in `~/.claude/plugins/npm-cache/` with
  // `npm_config_global` unset, and Claude Code already registers the plugin
  // for that path).
  if (process.env.npm_config_global === "true" || process.env.npm_config_global === "1") {
    try {
      execSync("memex install", { stdio: "inherit", timeout: 30000 });
    } catch {
      console.warn("memex install: not run automatically. Run `memex install` to register hooks.");
    }
    // npm 11 no longer fires `preuninstall`/`uninstall`/`postuninstall`, so we
    // can't auto-strip on `npm uninstall -g`. Surface the manual flow here so
    // the user knows to run `memex uninstall` BEFORE removing the binary.
    console.log("");
    console.log("memex ready. To uninstall later, run:");
    console.log("  memex uninstall                       # stop daemon, strip hooks/skills");
    console.log("  npm uninstall -g @xiongzubiao/memex   # remove the binary");
    return;
  }

  console.log("memex ready.");
}

main().catch((err) => {
  console.error("postinstall failed:", err.message);
  try {
    fs.writeFileSync(
      path.join(os.homedir(), ".memex", ".install-status"),
      JSON.stringify({ version: VERSION, timestamp: new Date().toISOString(), error: err.message }, null, 2),
    );
  } catch {}
  process.exit(1);
});
