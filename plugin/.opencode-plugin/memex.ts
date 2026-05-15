// @ts-nocheck — types resolve at runtime inside opencode (Bun + @opencode-ai/plugin).
/**
 * memex OpenCode plugin — fires `memex hook ingest opencode --session <id>`
 * whenever an OpenCode session goes idle (i.e. the assistant just finished a
 * turn).
 *
 * OpenCode has no dedicated "session ended" event — `session.idle` is the
 * closest equivalent and fires on every turn boundary. Re-ingesting after
 * each turn is cheap: memex computes a canonical content hash and exits
 * early when the session text hasn't changed since the last ingest.
 *
 * Spawned as a detached fire-and-forget child so the hook returns immediately
 * (matches the same pattern the other agent hooks use via `setsid`).
 */
import type { Plugin } from "@opencode-ai/plugin";

export const MemexPlugin: Plugin = async () => ({
  event: async ({ event }) => {
    if (event.type !== "session.idle") return;
    const sessionID = event.properties.sessionID;
    if (!sessionID) return;

    // Bun.spawn + detached: parent exits immediately, child survives.
    // stdio nulled so the child can't write to the user's terminal.
    Bun.spawn({
      cmd: ["memex", "hook", "ingest", "opencode", "--session", sessionID],
      stdin: "ignore",
      stdout: "ignore",
      stderr: "ignore",
      // setsid-equivalent: detach from the OpenCode session's process group.
      // If memex isn't on PATH, spawn rejects asynchronously; we don't await,
      // so the failure surfaces only in `opencode` logs — not user-visible.
    });
  },
});
