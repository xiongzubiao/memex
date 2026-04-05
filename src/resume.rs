use crate::types::SessionConfig;

/// Restored session state from SQLite memory.
pub struct SessionState {
    pub session_id: String,
    pub config: Option<SessionConfig>,
    pub task: Option<String>,
    pub last_draft: Option<String>,
    pub last_round: u32,
}

impl SessionState {
    /// Parse session state from memory entries.
    pub fn from_entries(
        session_id: &str,
        config_json: Option<&str>,
        task: Option<&str>,
        last_draft: Option<&str>,
        last_round: u32,
    ) -> Self {
        let config = config_json.and_then(|json| serde_json::from_str(json).ok());
        Self {
            session_id: session_id.to_string(),
            config,
            task: task.map(String::from),
            last_draft: last_draft.map(String::from),
            last_round,
        }
    }

    /// Whether this session can be resumed (has config + draft).
    pub fn is_resumable(&self) -> bool {
        self.config.is_some() && self.last_draft.is_some() && self.last_round > 0
    }
}

/// Load session state from memory.
pub async fn load_session(
    memory: &dyn zeroclaw::memory::Memory,
    session_id: &str,
) -> anyhow::Result<SessionState> {
    let config_key = format!("brainstorm:{}:config", session_id);
    let config_entry = memory.get(&config_key).await?;
    let config_json = config_entry.as_ref().map(|e| e.content.as_str());

    let task_key = format!("brainstorm:{}:task", session_id);
    let task_entry = memory.get(&task_key).await?;
    let task = task_entry.as_ref().map(|e| e.content.as_str());

    let mut last_round = 0u32;
    let mut last_draft = None;
    for round in 1..=20 {
        let draft_key = format!("brainstorm:{}:draft:{}", session_id, round);
        if let Ok(Some(entry)) = memory.get(&draft_key).await {
            last_round = round;
            last_draft = Some(entry.content.clone());
        } else {
            break;
        }
    }

    Ok(SessionState::from_entries(
        session_id,
        config_json,
        task,
        last_draft.as_deref(),
        last_round,
    ))
}
