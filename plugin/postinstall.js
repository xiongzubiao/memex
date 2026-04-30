const https = require("https");
const fs = require("fs");
const path = require("path");
const os = require("os");
const { execSync } = require("child_process");

const VERSION = "0.1.0";
// ort 2.0.0-rc.12 with api-23 targets ORT 1.23.x. Version 1.24 drops x86_64
// macOS, so 1.23.x is the last minor with full platform coverage.
const ORT_VERSION = "1.23.2";
const MODEL_URL = "https://huggingface.co/LeePark/gemma-embedding-300M-onnx-int8/resolve/main/model_int8.onnx";
// HuggingFace tokenizer.json for embeddinggemma-300m. Hosted in onnx-community
// because the upstream `google/embeddinggemma-300m` repo is gated; the
// onnx-community mirror has the same tokenizer.
const TOKENIZER_URL = "https://huggingface.co/onnx-community/embeddinggemma-300m-ONNX/resolve/main/tokenizer.json";
const RELEASE_BASE = `https://github.com/memverge/memex/releases/download/v${VERSION}`;
const ORT_RELEASE_BASE = `https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}`;

function getPlatformBinary() {
  const platform = os.platform();
  const arch = os.arch();
  const ext = platform === "win32" ? ".exe" : "";

  const key = `${platform}-${arch}`;
  const supported = {
    "darwin-arm64": `memex-darwin-arm64${ext}`,
    "darwin-x64": `memex-darwin-x64${ext}`,
    "linux-x64": `memex-linux-x64${ext}`,
    "win32-x64": `memex-windows-x64${ext}`,
  };

  const filename = supported[key];
  if (!filename) {
    console.error(`Unsupported platform: ${key}`);
    process.exit(1);
  }
  return filename;
}

function getOrtArchive() {
  const platform = os.platform();
  const arch = os.arch();
  const key = `${platform}-${arch}`;

  const archives = {
    "darwin-arm64": { file: `onnxruntime-osx-arm64-${ORT_VERSION}.tgz`, lib: "libonnxruntime.dylib" },
    "darwin-x64":  { file: `onnxruntime-osx-x86_64-${ORT_VERSION}.tgz`, lib: "libonnxruntime.dylib" },
    "linux-x64":   { file: `onnxruntime-linux-x64-${ORT_VERSION}.tgz`, lib: "libonnxruntime.so" },
    "win32-x64":   { file: `onnxruntime-win-x64-${ORT_VERSION}.zip`, lib: "onnxruntime.dll" },
  };

  return archives[key];
}

function download(url, dest) {
  return new Promise((resolve, reject) => {
    https.get(url, (res) => {
      if (res.statusCode === 302 || res.statusCode === 301) {
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

/** Recursively find files matching a prefix and copy them to dest. */
function copyLibFiles(dir, dest, prefix) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      copyLibFiles(full, dest, prefix);
    } else if (entry.name.startsWith(prefix)) {
      fs.copyFileSync(full, path.join(dest, entry.name));
    }
  }
}

/** Rewrite ${MEMEX_PLUGIN_DIR} placeholders in a hooks JSON to absolute path. */
function rewriteHooksFile(hooksPath, pluginDir) {
  if (!fs.existsSync(hooksPath)) return;
  const raw = fs.readFileSync(hooksPath, "utf8");
  const rewritten = raw.replace(/\$\{MEMEX_PLUGIN_DIR\}/g, pluginDir);
  if (rewritten !== raw) {
    fs.writeFileSync(hooksPath, rewritten);
  }
}

/** Check common system locations for an existing ONNX Runtime install. */
function findSystemOrt(libName) {
  // Respect explicit env var.
  if (process.env.ORT_DYLIB_PATH && fs.existsSync(process.env.ORT_DYLIB_PATH)) {
    return process.env.ORT_DYLIB_PATH;
  }
  const candidates = os.platform() === "win32"
    ? [
        // nuget / vcpkg / manual installs
        path.join(process.env.ProgramFiles || "C:\\Program Files", "onnxruntime", "lib"),
        path.join(process.env.LOCALAPPDATA || "", "onnxruntime", "lib"),
      ]
    : [
        // brew (macOS arm64 / x64)
        "/opt/homebrew/lib",
        "/usr/local/lib",
        // linux system
        "/usr/lib",
        "/usr/lib/x86_64-linux-gnu",
      ];
  for (const dir of candidates) {
    const p = path.join(dir, libName);
    if (fs.existsSync(p)) return p;
  }
  return null;
}

async function main() {
  const binDir = path.join(__dirname, "bin");
  const modelDir = path.join(os.homedir(), ".memex", "models");
  const libDir = path.join(os.homedir(), ".memex", "lib");

  fs.mkdirSync(binDir, { recursive: true });
  fs.mkdirSync(modelDir, { recursive: true });
  fs.mkdirSync(libDir, { recursive: true });

  // 1. Download memex binary.
  const binaryName = getPlatformBinary();
  const ext = os.platform() === "win32" ? ".exe" : "";
  const binaryDest = path.join(binDir, `memex${ext}`);

  if (!fs.existsSync(binaryDest)) {
    console.log(`Downloading memex binary (${binaryName})...`);
    await download(`${RELEASE_BASE}/${binaryName}`, binaryDest);
    if (os.platform() !== "win32") {
      fs.chmodSync(binaryDest, 0o755);
    }
    console.log("Binary downloaded.");
  }

  // 2. Download embedding model.
  const modelDest = path.join(modelDir, "embedding-gemma-300m.onnx");
  if (!fs.existsSync(modelDest)) {
    console.log("Downloading embedding model (~329MB)...");
    await download(MODEL_URL, modelDest);
    console.log("Model downloaded.");
  }

  // 2b. Download tokenizer.json for the embedding model. Without it,
  // memex falls back to a chars-as-tokens encoding that is ~4× slower
  // and produces lower-quality embeddings. Existing memex installs
  // upgrading from older versions should re-run this postinstall to
  // fetch the tokenizer.
  const tokenizerDest = path.join(modelDir, "embedding-gemma-300m-tokenizer.json");
  if (!fs.existsSync(tokenizerDest)) {
    console.log("Downloading tokenizer.json (~20MB)...");
    await download(TOKENIZER_URL, tokenizerDest);
    console.log("Tokenizer downloaded.");
  }

  // 3. Download ONNX Runtime shared library (skip if already installed).
  const ortInfo = getOrtArchive();
  if (ortInfo) {
    const libDest = path.join(libDir, ortInfo.lib);
    const systemOrt = findSystemOrt(ortInfo.lib);
    if (systemOrt) {
      console.log(`ONNX Runtime found at ${systemOrt} — skipping download.`);
    } else if (!fs.existsSync(libDest)) {
      const archiveDest = path.join(libDir, ortInfo.file);
      console.log(`Downloading ONNX Runtime v${ORT_VERSION}...`);
      await download(`${ORT_RELEASE_BASE}/${ortInfo.file}`, archiveDest);

      // Extract the shared library from the archive.
      console.log("Extracting ONNX Runtime...");
      const tmpDir = path.join(libDir, "_ort_tmp");
      fs.mkdirSync(tmpDir, { recursive: true });
      if (ortInfo.file.endsWith(".tgz")) {
        execSync(`tar xzf "${archiveDest}" -C "${tmpDir}"`, { stdio: "pipe" });
      } else {
        // Windows: use PowerShell to extract .zip
        execSync(`powershell -Command "Expand-Archive -Path '${archiveDest}' -DestinationPath '${tmpDir}'"`, { stdio: "pipe" });
      }
      // Walk the temp dir and copy matching library files (cross-platform).
      copyLibFiles(tmpDir, libDir, ortInfo.lib);
      fs.rmSync(tmpDir, { recursive: true, force: true });
      fs.rmSync(archiveDest, { force: true });
      console.log("ONNX Runtime ready.");
    }
  }

  // 4. Rewrite ${MEMEX_PLUGIN_DIR} placeholders in hooks JSON to absolute paths.
  const PLUGIN_DIR = __dirname;
  rewriteHooksFile(path.join(PLUGIN_DIR, "hooks", "claude-code.json"), PLUGIN_DIR);
  rewriteHooksFile(path.join(PLUGIN_DIR, "hooks", "codex.json"), PLUGIN_DIR);
  rewriteHooksFile(path.join(PLUGIN_DIR, "hooks", "gemini-cli.json"), PLUGIN_DIR);

  // 5. Stop any running memex daemon so the next request loads the new binary.
  try {
    execSync(`${JSON.stringify(binaryDest)} daemon stop`, { stdio: "pipe" });
    console.log("Stopped any running memex daemon (will relaunch on next CLI use)");
  } catch (e) {
    // No-op: daemon wasn't running, or stop failed harmlessly.
  }

  console.log("memex ready.");
}

main().catch((err) => {
  console.error("postinstall failed:", err.message);
  process.exit(1);
});
