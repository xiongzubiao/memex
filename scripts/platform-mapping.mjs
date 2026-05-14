import os from "node:os";

// v0.1.0 ships only Unix platforms (darwin-arm64, darwin-x64, linux-x64, linux-arm64).
// Windows is rejected by the wrapper before reaching here.
export function platformPackageName() {
  return `@xiongzubiao/memex-${os.platform()}-${os.arch()}`;
}
