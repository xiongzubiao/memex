//! `memex hook ingest <agent>` — receive a session-end hook payload on stdin,
//! pluck `transcript_path`, hand off to the daemon-ingest code path. Replaces
//! the three node shim scripts that used to live under `plugin/hooks/`.

use anyhow::{Context, Result, anyhow};
use std::io::Read;

/// Returns Ok(None) when `MEMEX_INTERNAL=1` is set (the caller should exit 0
/// without ingesting; used by recursive memex invocations to break loops).
pub fn read_transcript_path() -> Result<Option<String>> {
    if std::env::var("MEMEX_INTERNAL").as_deref() == Ok("1") {
        return Ok(None);
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("read hook payload from stdin")?;
    if buf.trim().is_empty() {
        return Err(anyhow!("empty hook payload on stdin"));
    }
    let payload: serde_json::Value =
        serde_json::from_str(&buf).context("parse hook payload as JSON")?;
    let path = payload
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing transcript_path in hook payload"))?;
    if path.is_empty() {
        return Err(anyhow!("empty transcript_path in hook payload"));
    }
    Ok(Some(path.to_string()))
}
