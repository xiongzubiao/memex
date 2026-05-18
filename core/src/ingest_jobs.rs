//! Ingest-job lifecycle + retention. Daemon-side state for the
//! transcript / document ingest pipelines: pending → processing →
//! completed / failed, plus the LLM cache prune that lives next to
//! it because both fire from the same daemon-startup pass.
//!
//! Methods are inherent on `Db` so call sites read the same as the
//! rest of the DB API (`db.insert_ingest_job(...)`).

use crate::error::Result;
use crate::search::{Db, mutex_err, now_rfc3339, sqlite_err};

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

impl Db {
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
        // Re-submit of a previously-failed row resets to pending;
        // completed/processing rows are left alone so we neither
        // re-run a success nor race an in-flight job.
        conn.execute(
            "INSERT INTO ingest_jobs \
             (job_id, job_type, source_path, agent, content_hash, collections, status, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8) \
             ON CONFLICT(job_id) DO UPDATE \
               SET status = 'pending', error = NULL, updated_at = ?8 \
               WHERE ingest_jobs.status = 'failed'",
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
        let mut stmt = conn
            .prepare(
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
            let job_type = JobType::from_str(&job_type_s).ok_or_else(|| {
                crate::error::MemexError::Internal(format!("bad job_type: {job_type_s}"))
            })?;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_jobs_inserts_and_lists_transcript_job() {
        let dir = tempfile::TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let s = memex.search();
        s.insert_ingest_job(
            "job-t1",
            JobType::Transcript,
            "/abs/path/to/session.jsonl",
            Some("claude-code"),
            "deadbeef".repeat(8).as_str(),
            &["default".to_string()],
        )
        .unwrap();
        let jobs = s.pending_ingest_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].job_id, "job-t1");
        assert_eq!(jobs[0].job_type, JobType::Transcript);
        assert_eq!(jobs[0].agent.as_deref(), Some("claude-code"));
    }

    #[test]
    fn ingest_jobs_inserts_and_lists_document_job() {
        let dir = tempfile::TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let s = memex.search();
        s.insert_ingest_job(
            "job-d1",
            JobType::Document,
            "https://example.com/post",
            None,
            "cafebabe".repeat(8).as_str(),
            &["team-a".to_string(), "incidents".to_string()],
        )
        .unwrap();
        let jobs = s.pending_ingest_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].job_type, JobType::Document);
        assert_eq!(jobs[0].agent, None);
        assert_eq!(jobs[0].collections, vec!["team-a", "incidents"]);
    }

    /// Daemon startup must transition every pending/processing job to
    /// failed with the canonical "interrupted by daemon restart"
    /// reason and bump updated_at. Completed/failed rows must NOT be
    /// touched. This invariant is load-bearing: the 30-day prune is
    /// keyed off updated_at, so a restart pushing the field forward
    /// gives the user the full retention window to inspect what
    /// failed before the row is reaped.
    #[test]
    fn recover_stuck_ingest_jobs_only_touches_in_flight_rows() {
        let dir = tempfile::TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let s = memex.search();
        s.insert_ingest_job(
            "job-pending",
            JobType::Transcript,
            "/p1.jsonl",
            None,
            "00".repeat(32).as_str(),
            &[],
        )
        .unwrap();
        s.insert_ingest_job(
            "job-processing",
            JobType::Transcript,
            "/p2.jsonl",
            None,
            "11".repeat(32).as_str(),
            &[],
        )
        .unwrap();
        s.update_ingest_job_status("job-processing", "processing", None)
            .unwrap();
        s.insert_ingest_job(
            "job-completed",
            JobType::Transcript,
            "/p3.jsonl",
            None,
            "22".repeat(32).as_str(),
            &[],
        )
        .unwrap();
        s.update_ingest_job_status("job-completed", "completed", None)
            .unwrap();
        s.insert_ingest_job(
            "job-failed",
            JobType::Transcript,
            "/p4.jsonl",
            None,
            "33".repeat(32).as_str(),
            &[],
        )
        .unwrap();
        s.update_ingest_job_status("job-failed", "failed", Some("worker died"))
            .unwrap();

        let recovered = s.recover_stuck_ingest_jobs().unwrap();
        assert_eq!(recovered, 2, "pending + processing should transition");

        let row_status = |job_id: &str| -> (String, Option<String>) {
            s.with_connection(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT status, error FROM ingest_jobs WHERE job_id=?1",
                        rusqlite::params![job_id],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                    )
                    .unwrap())
            })
            .unwrap()
        };
        assert_eq!(row_status("job-pending").0, "failed");
        assert_eq!(row_status("job-processing").0, "failed");
        assert_eq!(
            row_status("job-pending").1.as_deref(),
            Some("interrupted by daemon restart")
        );
        assert_eq!(row_status("job-completed").0, "completed");
        assert_eq!(row_status("job-failed").1.as_deref(), Some("worker died"));
    }

    /// Re-submitting a previously-failed job resets to pending so
    /// stale `interrupted by daemon restart` rows are retryable.
    /// Completed and processing rows must not be touched.
    #[test]
    fn insert_ingest_job_resets_failed_rows_to_pending() {
        let dir = tempfile::TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let s = memex.search();
        let args = (
            JobType::Transcript,
            "/p.jsonl",
            None::<&str>,
            "aa".repeat(32),
            vec!["default".to_string()],
        );

        // Seed three rows in different terminal/active states.
        for jid in ["job-failed", "job-completed", "job-processing"] {
            s.insert_ingest_job(jid, args.0, args.1, args.2, &args.3, &args.4)
                .unwrap();
        }
        s.update_ingest_job_status("job-failed", "failed", Some("worker died"))
            .unwrap();
        s.update_ingest_job_status("job-completed", "completed", None)
            .unwrap();
        s.update_ingest_job_status("job-processing", "processing", None)
            .unwrap();

        // Re-insert each. The failed row should flip to pending and
        // clear its error; the other two should be left alone.
        for jid in ["job-failed", "job-completed", "job-processing"] {
            s.insert_ingest_job(jid, args.0, args.1, args.2, &args.3, &args.4)
                .unwrap();
        }
        let row_status = |job_id: &str| -> (String, Option<String>) {
            s.with_connection(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT status, error FROM ingest_jobs WHERE job_id=?1",
                        rusqlite::params![job_id],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                    )
                    .unwrap())
            })
            .unwrap()
        };
        assert_eq!(row_status("job-failed"), ("pending".to_string(), None));
        assert_eq!(row_status("job-completed").0, "completed");
        assert_eq!(row_status("job-processing").0, "processing");
    }

    /// Prune deletes terminal rows older than the cutoff and leaves
    /// recent ones plus in-flight rows alone.
    #[test]
    fn prune_terminal_ingest_jobs_keyed_off_updated_at() {
        let dir = tempfile::TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let s = memex.search();
        s.insert_ingest_job(
            "old",
            JobType::Transcript,
            "/old.jsonl",
            None,
            "aa".repeat(32).as_str(),
            &[],
        )
        .unwrap();
        s.update_ingest_job_status("old", "completed", None)
            .unwrap();
        s.insert_ingest_job(
            "new",
            JobType::Transcript,
            "/new.jsonl",
            None,
            "bb".repeat(32).as_str(),
            &[],
        )
        .unwrap();
        s.update_ingest_job_status("new", "completed", None)
            .unwrap();
        s.with_connection(|conn| {
            conn.execute(
                "UPDATE ingest_jobs SET updated_at = datetime('now', '-60 days') \
                 WHERE job_id = 'old'",
                [],
            )?;
            Ok(())
        })
        .unwrap();

        let pruned = s.prune_terminal_ingest_jobs(30).unwrap();
        assert_eq!(pruned, 1, "only `old` should be pruned at 30-day cutoff");
        let remaining: Vec<String> = s
            .with_connection(|conn| {
                let mut stmt = conn
                    .prepare("SELECT job_id FROM ingest_jobs ORDER BY job_id")
                    .unwrap();
                let rows: Vec<String> = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .collect();
                Ok(rows)
            })
            .unwrap();
        assert_eq!(remaining, vec!["new".to_string()]);
    }
}
