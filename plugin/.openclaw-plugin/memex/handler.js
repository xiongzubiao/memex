// OpenClaw hook: on /new or /reset, dispatch the previous session transcript
// to memex via `memex ingest --agent openclaw --source <path>` with content
// piped on stdin. Fire-and-forget so the command acknowledgement isn't
// delayed by the LLM extract pipeline.
//
// OpenClaw fires this hook BEFORE archiving the prior transcript (the rename
// to `<base>.jsonl.reset.<ts>` happens microseconds later). If we hand the
// live path to `memex ingest`, the subprocess opens it after the archive
// and reads the new empty session. Reading the bytes synchronously here and
// piping them through stdin sidesteps the race entirely — no temp file,
// no leak.

import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";

const memexOpenClawHook = (event) => {
  if (event?.type !== "command") return;
  if (event.action !== "new" && event.action !== "reset") return;

  const context = event.context || {};
  const sessionEntry = context.previousSessionEntry || context.sessionEntry || {};
  const sessionFile = sessionEntry.sessionFile;
  if (!sessionFile) return;

  let content;
  try {
    content = readFileSync(sessionFile);
  } catch {
    return;
  }

  try {
    const child = spawn(
      "memex",
      ["ingest", "--agent", "openclaw", "--source", sessionFile],
      { detached: true, stdio: ["pipe", "ignore", "ignore"] },
    );
    child.on("error", () => {
      // memex not on PATH or spawn failed; silent — the hook shouldn't
      // disrupt OpenClaw's command pipeline.
    });
    child.stdin.write(content);
    child.stdin.end();
    child.unref();
  } catch {
    // Same: never throw out of the hook.
  }
};

export default memexOpenClawHook;
