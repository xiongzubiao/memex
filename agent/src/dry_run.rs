use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use zeroclaw::providers::Provider;

/// Factory function type that creates a boxed Provider from a provider name.
pub type ProviderFactory = Arc<dyn Fn(&str) -> anyhow::Result<Box<dyn Provider>> + Send + Sync>;

/// A mock provider that returns canned responses for dry-run testing.
pub struct DryRunProvider;

#[async_trait]
impl Provider for DryRunProvider {
    async fn chat_with_system(
        &self,
        _system: Option<&str>,
        prompt: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(generate_response(prompt))
    }
}

#[async_trait]
impl memex_core::LlmProvider for DryRunProvider {
    async fn chat(
        &self,
        _system: Option<&str>,
        message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(generate_response(message))
    }
}

/// Create a ProviderFactory that returns DryRunProvider for any provider name.
pub fn dry_run_provider_factory() -> ProviderFactory {
    Arc::new(|_name: &str| Ok(Box::new(DryRunProvider) as Box<dyn Provider>))
}

/// A mock provider that fails with "401 Unauthorized" after N successful calls.
pub struct AuthFailProvider {
    calls: AtomicU32,
    fail_after: u32,
}

impl AuthFailProvider {
    pub fn new(successes_before_fail: u32) -> Self {
        Self {
            calls: AtomicU32::new(0),
            fail_after: successes_before_fail,
        }
    }
}

#[async_trait]
impl Provider for AuthFailProvider {
    async fn chat_with_system(
        &self,
        _system: Option<&str>,
        prompt: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let count = self.calls.fetch_add(1, Ordering::SeqCst);
        if count >= self.fail_after {
            anyhow::bail!("401 Unauthorized: Invalid API key")
        }
        DryRunProvider
            .chat_with_system(_system, prompt, _model, _temperature)
            .await
    }
}

/// Create a ProviderFactory that returns AuthFailProvider instances.
pub fn auth_fail_provider_factory(successes_before_fail: u32) -> ProviderFactory {
    Arc::new(move |_name: &str| {
        Ok(Box::new(AuthFailProvider::new(successes_before_fail)) as Box<dyn Provider>)
    })
}

/// A mock provider that always returns "429 Too Many Requests".
pub struct RateLimitProvider;

#[async_trait]
impl Provider for RateLimitProvider {
    async fn chat_with_system(
        &self,
        _system: Option<&str>,
        _prompt: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("429 Too Many Requests"))
    }
}

/// Create a ProviderFactory that returns RateLimitProvider for any provider name.
pub fn rate_limit_provider_factory() -> ProviderFactory {
    Arc::new(|_name: &str| Ok(Box::new(RateLimitProvider) as Box<dyn Provider>))
}

const AGENT_CONSULT_RESPONSE: &str = r#"## Problem
The rate-limiting API should use token bucket algorithm for its simplicity and efficiency.

## Architecture
A middleware layer that intercepts requests and checks against per-client token buckets stored in Redis.

## API
- POST /api/rate-limit/check — check if request is allowed
- GET /api/rate-limit/status — current limit status

## Data Model
Token bucket: { client_id, tokens, last_refill, max_tokens, refill_rate }

## Error Handling
Return 429 Too Many Requests with Retry-After header.

## Testing
Unit tests for token bucket logic. Integration tests with Redis. Load tests for concurrent access."#;

const AGENT_REVIEW_RESPONSE: &str = r#"## Section Evaluations

### Problem: APPROVE
Clear problem statement.

### Architecture: MINOR
The middleware approach is sound. Consider adding a fallback for Redis outages.

### API: APPROVE
Clean REST design.

### Data Model: APPROVE
Token bucket is appropriate.

### Error Handling: MINOR
Add rate limit headers (X-RateLimit-Remaining) to all responses, not just 429s.

### Testing: APPROVE
Good coverage plan.

## Overall
Score: 8/10. Minor improvements needed in Architecture and Error Handling."#;

fn generate_response(prompt: &str) -> String {
    let lower = prompt.to_lowercase();

    // --- Copilot agent prompts (check first, before brainstorm patterns) ---
    // These match the copilot prompt templates injected via format_copilot_prompt().
    if lower.contains("knowledge retrieval assistant") || lower.contains("query_copilot") {
        return COPILOT_QUERY_RESPONSE.to_string();
    }
    if lower.contains("knowledge curator for a personal wiki") || lower.contains("ingest_copilot") {
        return COPILOT_INGEST_RESPONSE.to_string();
    }
    if lower.contains("wiki health checker") || lower.contains("lint_copilot") {
        return COPILOT_LINT_RESPONSE.to_string();
    }
    if lower.contains("multi-model brainstorming session") || lower.contains("brainstorm_copilot") {
        return BRAINSTORM_RESPONSE.to_string();
    }

    // --- Agent sub-agent responses (swarm panelists) ---
    if lower.contains("brainstorming panelist") || lower.contains("generate a thoughtful") {
        return AGENT_CONSULT_RESPONSE.to_string();
    }
    if lower.contains("critical reviewer") || lower.contains("evaluate a draft") {
        return AGENT_REVIEW_RESPONSE.to_string();
    }

    // --- Memex core pipeline prompts (autopilot: ingest, query, lint) ---
    if lower.contains("updating a personal wiki") || lower.contains("wiki maintainer") {
        MEMEX_INGEST_RESPONSE.to_string()
    } else if lower.contains("which wiki pages are most relevant")
        || lower.contains("which wiki pages are relevant")
    {
        MEMEX_QUERY_SELECT_RESPONSE.to_string()
    } else if lower.contains("based on these wiki pages") {
        MEMEX_QUERY_ANSWER_RESPONSE.to_string()
    } else if lower.contains("auditing a wiki") {
        MEMEX_LINT_RESPONSE.to_string()
    // --- Brainstormer pipeline prompts ---
    } else if lower.contains("brainstorm")
        || lower.contains("propose")
        || lower.contains("approaches")
    {
        BRAINSTORM_RESPONSE.to_string()
    } else if lower.contains("synthesize")
        || lower.contains("merge")
        || lower.contains("outputs to merge")
    {
        MERGE_RESPONSE.to_string()
    } else if lower.contains("reviewing")
        || lower.contains("critique")
        || lower.contains("evaluation dimensions")
    {
        REVIEW_RESPONSE.to_string()
    } else if lower.contains("verify")
        || lower.contains("merge_quality")
        || lower.contains("critiques that should")
    {
        QUALITY_RESPONSE.to_string()
    } else if lower.contains("final review")
        || lower.contains("finalize")
        || lower.contains("check for")
    {
        "CLEAN".to_string()
    } else {
        format!("[dry-run] Echo: {}...", &prompt[..prompt.len().min(100)])
    }
}

// --- Copilot agent mock responses ---

const COPILOT_QUERY_RESPONSE: &str = r#"I searched the wiki using memory_recall and found relevant pages.

Based on the wiki content, here is what I found:

The wiki currently contains information about various topics ingested from your sources. I can see entries related to your question in the index.

Would you like me to:
1. Read specific wiki pages for more detail?
2. Search for related topics?
3. Add any new insights to the wiki?

Type your follow-up question or 'done' to finish."#;

const COPILOT_INGEST_RESPONSE: &str = r#"I've read the source file(s) using file_read. Here are the key takeaways:

**Entities identified:**
- Topic A: Core concept from the source
- Topic B: Related entity mentioned in the source

**Key insights:**
- The source describes relationships between Topic A and Topic B
- There are specific claims that could be extracted as wiki pages

I'd like to create the following wiki pages:
1. `wiki/topic-a.md` (tags: entity) - Core concept
2. `wiki/topic-b.md` (tags: entity) - Related entity

Should I proceed with creating these pages, or would you like to adjust the emphasis?"#;

const COPILOT_LINT_RESPONSE: &str = r#"I've analyzed the wiki pages. Here are the health check results:

**Issues found: 0 critical, 1 minor**

Minor:
- Some pages could benefit from additional cross-references

**Suggestions:**
- Consider ingesting more sources to expand coverage
- The wiki would benefit from a dedicated page on [suggested topic]

No fixes needed at this time. Would you like me to look at anything specific?"#;

const BRAINSTORM_RESPONSE: &str = r#"
## Problem
The social media feed cache needs to handle millions of concurrent users with sub-100ms latency.

### Approach 1: Redis Cluster with Consistent Hashing
- Use Redis Cluster with hash slots for automatic sharding
- Pros: Battle-tested, sub-millisecond reads, built-in replication
- Cons: Memory-bound, complex cluster management

### Approach 2: Tiered Cache (L1 Local + L2 Distributed)
- L1: In-process LRU cache (Caffeine/Guava) for hot feeds
- L2: Redis/Memcached for warm data
- Pros: Reduces network hops for popular feeds, graceful degradation
- Cons: Cache coherence complexity, more moving parts

## Architecture
### Approach 1: Event-Driven with CQRS
- Separate read/write paths
- Write: Kafka events -> cache invalidation
- Read: Cache-first with DB fallback

### Approach 2: Cache-Aside with Write-Through
- Application manages cache explicitly
- On write: update DB, then invalidate cache
- On read: check cache, fallback to DB, populate cache

## API
### Approach 1: GraphQL with DataLoader
- Batch feed items per request using DataLoader pattern
- Cache at the resolver level

### Approach 2: REST with Cursor Pagination
- GET /feed?cursor=xxx&limit=20
- Cache entire page responses by cursor

## Data Model
### Approach 1: Denormalized Feed Documents
- Store pre-computed feed as JSON documents

### Approach 2: Normalized with Materialized Views
- Store posts, follows, interactions separately

## Error Handling
- Circuit breaker pattern for cache failures
- Fallback to DB with degraded latency SLA
- Retry with exponential backoff for transient errors

## Testing
- Load test with k6: 10K concurrent users, p99 < 200ms
- Chaos engineering: kill Redis nodes, verify failover
- Integration tests with Testcontainers
"#;

const MERGE_RESPONSE: &str = r#"
# Distributed Cache System for Social Media Feed

## Problem
The social media feed cache must serve millions of concurrent users with sub-100ms read latency and graceful degradation under failure.

## Architecture
Tiered cache with event-driven invalidation:
- **L1 (Local)**: In-process LRU cache on each app server. TTL: 30 seconds.
- **L2 (Distributed)**: Redis Cluster with consistent hashing. TTL: 5 minutes.
- **Invalidation**: Kafka consumers trigger cache updates on writes.

## API
REST with cursor-based pagination:
```
GET /v1/feed?cursor=<opaque>&limit=20
Response: { items: [...], next_cursor: "...", cached: true }
```

## Data Model
Hybrid: denormalized feed documents in Redis for hot path, normalized PostgreSQL tables for warm path.
Schema: `feed:{user_id}:{page}` -> JSON array of post summaries.

## Error Handling
- Circuit breaker: trip after 5 consecutive Redis failures, fallback to DB.
- Retry with exponential backoff and jitter.
- Serve stale cache when DB is under pressure.

## Testing
- Load testing with k6 (10K users, p99 < 200ms target).
- Chaos engineering for Redis failover.
- Integration tests with Testcontainers.
"#;

const REVIEW_RESPONSE: &str = r#"
### Problem
**Score**: better
**Critique**: Clear requirements. Could specify write volume.
**Suggestion**: Add expected write volume: 50K posts/minute.

### Architecture
**Score**: better
**Critique**: Solid tiered approach. Missing cache warming strategy.
**Suggestion**: Pre-populate L1 for top 10K users on startup.

### API
**Score**: same
**Critique**: Cursor format unspecified.
**Suggestion**: Define cursor as base64-encoded (timestamp, post_id).

### Data Model
**Score**: better
**Critique**: Feed document size could grow unbounded.
**Suggestion**: Cap at 100 items per page with metadata key.

### Error Handling
**Score**: better
**Critique**: Missing partial failure handling.
**Suggestion**: Log inconsistency events, schedule reconciliation.

### Testing
**Score**: same
**Critique**: Missing CI performance regression tests.
**Suggestion**: Nightly benchmark that fails on >10% p99 regression.
"#;

const QUALITY_RESPONSE: &str = "All critiques addressed. PASS";

// Memex-specific mock responses

const MEMEX_INGEST_RESPONSE: &str = r#"
<<< PAGE: wiki/dry-run-summary.md >>>
<<< ACTION: create >>>
---
title: Dry Run Source Summary
tags:
  - source-summary
created: 2026-04-06T00:00:00Z
last_updated: 2026-04-06T00:00:00Z
sources:
  - sources/documents/dry-run-source
---

Summary of the ingested source (dry-run mode).
<<< END PAGE >>>

<<< PAGE: wiki/dry-run-topic.md >>>
<<< ACTION: create >>>
---
title: Dry Run Topic
tags:
  - entity
created: 2026-04-06T00:00:00Z
last_updated: 2026-04-06T00:00:00Z
sources:
  - sources/documents/dry-run-source
---

Key concepts extracted from the source (dry-run mode).
<<< END PAGE >>>
"#;

const MEMEX_QUERY_SELECT_RESPONSE: &str = "wiki/dry-run-topic.md\n";

const MEMEX_QUERY_ANSWER_RESPONSE: &str =
    "Based on the wiki: key concepts were extracted from the source. [Dry Run Topic]";

const MEMEX_LINT_RESPONSE: &str = r#"ISSUES:

SUGGESTED_QUESTIONS:
- What other topics should be explored?

SUGGESTED_SOURCES:
- Additional documentation on the topic
"#;
