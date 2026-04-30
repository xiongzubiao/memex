//! Ingest-job lifecycle + retention. Daemon-side state for the
//! transcript / document ingest pipelines: pending → processing →
//! completed / failed, plus the LLM cache prune that lives next to
//! it because both fire from the same daemon-startup pass.
//!
//! Pulled out of `search/mod.rs` so the search module isn't carrying
//! ingest-pipeline concerns in addition to the BM25 / FTS / chunks
//! plumbing it owns. Methods are inherent on `Bm25Search` so call
//! sites read the same as before (`search.insert_ingest_job(...)`).

use crate::error::Result;

use super::{Bm25Search, mutex_err, now_rfc3339, sqlite_err};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobType {
    Transcript,
    Document,
}

impl JobType {
    pub fn as_str(self) -> &'static str {
        match self {
            JobType::Transcript => "transcript",
            JobType::Document => "document",
        }
    }
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "transcript" => Some(JobType::Transcript),
            "document" => Some(JobType::Document),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingJob {
    pub job_id: String,
    pub job_type: JobType,
    pub source_path: String,
    pub agent: Option<String>,
    pub content_hash: String,
    pub collections: Vec<String>,
}

impl Bm25Search {
    pub fn insert_ingest_job(
        &self,
        job_id: &str,
        job_type: JobType,
        source_path: &str,
        agent: Option<&str>,
        content_hash: &str,
        collections: &[String],
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let now = now_rfc3339();
        let collections_json = serde_json::to_string(collections)
            .map_err(|e| crate::error::MemexError::Internal(format!("collections json: {e}")))?;
        conn.execute(
            "INSERT OR IGNORE INTO ingest_jobs \
             (job_id, job_type, source_path, agent, content_hash, collections, status, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8)",
            rusqlite::params![
                job_id,
                job_type.as_str(),
                source_path,
                agent,
                content_hash,
                collections_json,
                &now,
                &now
            ],
        )
        .map_err(sqlite_err)?;
        Ok(())
    }

    pub fn update_ingest_job_status(
        &self,
        job_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let now = now_rfc3339();
        conn.execute(
            "UPDATE ingest_jobs SET status = ?1, updated_at = ?2, error = ?3 WHERE job_id = ?4",
            rusqlite::params![status, &now, error, job_id],
        )
        .map_err(sqlite_err)?;
        Ok(())
    }

    /// Mark every `pending`/`processing` ingest job as `failed` with the
    /// reason "interrupted by daemon restart" and bump `updated_at`.
    /// Called on daemon startup: any row still in flight at startup
    /// belongs to a previous daemon that crashed or was killed before
    /// its worker handed back a result. Returns the number of rows
    /// transitioned. The 30-day prune is keyed off `updated_at` so the
    /// freshly-failed rows live for the full retention window before
    /// being cleaned up.
    pub fn recover_stuck_ingest_jobs(&self) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let n = conn
            .execute(
                "UPDATE ingest_jobs \
                 SET status = 'failed', \
                     error = 'interrupted by daemon restart', \
                     updated_at = datetime('now') \
                 WHERE status IN ('pending', 'processing')",
                [],
            )
            .map_err(sqlite_err)?;
        Ok(n)
    }

    /// DELETE terminal (`completed`/`failed`) ingest jobs whose
    /// `updated_at` is older than the given number of days. Returns
    /// number of rows pruned.
    pub fn prune_terminal_ingest_jobs(&self, days: i64) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let cutoff = format!("-{days} days");
        let n = conn
            .execute(
                "DELETE FROM ingest_jobs \
                 WHERE status IN ('completed', 'failed') \
                   AND updated_at < datetime('now', ?1)",
                rusqlite::params![cutoff],
            )
            .map_err(sqlite_err)?;
        Ok(n)
    }

    /// DELETE llm_cache rows older than `days`, keyed off `created_at`.
    /// Returns rows pruned. Cache entries are model-pinned via
    /// `cache_key`, so model upgrades age old entries naturally; this
    /// pass is the long-tail cleanup for unused keys.
    pub fn prune_llm_cache(&self, days: i64) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let cutoff = format!("-{days} days");
        let n = conn
            .execute(
                "DELETE FROM llm_cache WHERE created_at < datetime('now', ?1)",
                rusqlite::params![cutoff],
            )
            .map_err(sqlite_err)?;
        Ok(n)
    }

    pub fn pending_ingest_jobs(&self) -> Result<Vec<PendingJob>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn.prepare(
            "SELECT job_id, job_type, source_path, agent, content_hash, collections \
             FROM ingest_jobs WHERE status IN ('pending', 'processing')",
        )
        .map_err(sqlite_err)?;
        let rows = stmt
            .query_map([], |row| {
                let job_type_s: String = row.get(1)?;
                let collections_s: String = row.get(5)?;
                Ok((
                    row.get::<_, String>(0)?,
                    job_type_s,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    collections_s,
                ))
            })
            .map_err(sqlite_err)?;
        let mut jobs = Vec::new();
        for row in rows {
            let (job_id, job_type_s, source_path, agent, content_hash, collections_s) =
                row.map_err(sqlite_err)?;
            let job_type = JobType::from_str(&job_type_s)
                .ok_or_else(|| crate::error::MemexError::Internal(format!("bad job_type: {job_type_s}")))?;
            let collections: Vec<String> = serde_json::from_str(&collections_s).unwrap_or_default();
            jobs.push(PendingJob {
                job_id,
                job_type,
                source_path,
                agent,
                content_hash,
                collections,
            });
        }
        Ok(jobs)
    }

    /// Returns true if any ingest-produced document with the given
    /// content hash already finished ingestion. Gates on the
    /// `ingest_jobs` table (status='completed'), not on raw row
    /// existence: if a prior attempt stored the raw doc but crashed
    /// before producing wiki pages, the raw row exists alone with no
    /// completed job. Dedup-on-raw-existence would short-circuit the
    /// retry forever, leaving the user with a stored raw and zero
    /// wiki pages and no recovery path. Gating on completion lets a
    /// retry replay the wiki extraction; the raw upsert is
    /// idempotent.
    pub fn ingest_dedup_exists(&self, content_hash: &str) -> Result<bool> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ingest_jobs \
                 WHERE content_hash = ?1 AND status = 'completed'",
                rusqlite::params![content_hash],
                |r| r.get(0),
            )
            .map_err(sqlite_err)?;
        Ok(count > 0)
    }
}
