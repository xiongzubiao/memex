#!/usr/bin/env node
// plugin/hooks/claude-code-session-end.js
// Reads the SessionEnd hook payload (JSON on stdin), extracts transcript_path,
// invokes `memex ingest --agent claude-code <transcript-path>`.

const fs = require('fs');
const { spawnSync } = require('child_process');

if (process.env.MEMEX_INTERNAL === '1') process.exit(0);

let payload;
try {
  payload = JSON.parse(fs.readFileSync(0, 'utf8'));
} catch (e) {
  console.error(`memex hook: failed to parse stdin JSON: ${e.message}`);
  process.exit(1);
}

const transcriptPath = payload && payload.transcript_path;
if (typeof transcriptPath !== 'string' || !transcriptPath) {
  console.error('memex hook: missing transcript_path in payload');
  process.exit(1);
}

const r = spawnSync('memex',
  ['ingest', '--agent', 'claude-code', transcriptPath],
  { stdio: 'inherit' });
if (r.error) console.error(`memex hook: spawn failed: ${r.error.message}`);
process.exit(r.status ?? 1);
