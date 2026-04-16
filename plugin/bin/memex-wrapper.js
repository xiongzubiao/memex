#!/usr/bin/env node
const { execFileSync } = require("child_process");
const path = require("path");
const os = require("os");

const ext = os.platform() === "win32" ? ".exe" : "";
const binary = path.join(__dirname, `memex${ext}`);

try {
  execFileSync(binary, process.argv.slice(2), { stdio: "inherit" });
} catch (e) {
  if (e.signal) process.exit(128);
  if (typeof e.status === "number") process.exit(e.status);
  console.error("memex binary not found. Run: npm run postinstall");
  process.exit(1);
}
