use crate::types::*;
use std::path::Path;

/// Format the final document with a YAML metadata header.
pub fn format_export(
    task: &str,
    config: &SessionConfig,
    document: &str,
    rounds: u32,
    converged: bool,
    cost_usd: f64,
) -> String {
    let models: Vec<String> = config
        .brainstorm_models
        .iter()
        .map(|m| format!("{}/{}", m.provider, m.model))
        .collect();

    format!(
        "---\ntask: {}\ntype: {}\nmode: {:?}\nmodels:\n{}\nrounds: {}\nconverged: {}\ncost_usd: {:.2}\ngenerated: {}\n---\n\n{}",
        task,
        config.task_type,
        config.mode,
        models.iter().map(|m| format!("  - {}", m)).collect::<Vec<_>>().join("\n"),
        rounds,
        converged,
        cost_usd,
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
        document,
    )
}

/// Write the export to a file.
pub fn write_export(path: &Path, content: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}
