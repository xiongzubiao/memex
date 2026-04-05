use crate::types::Preset;
use std::collections::HashMap;

/// Replace {{variable}} placeholders with values from the map.
/// User-provided values are escaped to prevent injection of template variables.
pub fn interpolate(template: &str, vars: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in vars {
        // Escape any {{ in the value to prevent injection
        let safe_value = value.replace("{{", "{ {");
        result = result.replace(&format!("{{{{{}}}}}", key), &safe_value);
    }
    result
}

/// Return an embedded preset TOML string by name.
fn embedded_preset(name: &str) -> Option<&'static str> {
    match name {
        "software" => Some(include_str!("../presets/software_design.toml")),
        "general" => Some(include_str!("../presets/general.toml")),
        "research" => Some(include_str!("../presets/research.toml")),
        "article" => Some(include_str!("../presets/article.toml")),
        "book" => Some(include_str!("../presets/book.toml")),
        "strategy" => Some(include_str!("../presets/strategy.toml")),
        _ => None,
    }
}

/// Return an embedded prompt template string by name.
fn embedded_prompt(name: &str) -> Option<&'static str> {
    match name {
        "brainstorm" => Some(include_str!("../prompts/brainstorm.md")),
        "merge" => Some(include_str!("../prompts/merge.md")),
        "review" => Some(include_str!("../prompts/review.md")),
        "finalize" => Some(include_str!("../prompts/finalize.md")),
        "merge_quality" => Some(include_str!("../prompts/merge_quality.md")),
        "evaluate" => Some(include_str!("../prompts/evaluate.md")),
        "convergence_semantic" => Some(include_str!("../prompts/convergence_semantic.md")),
        _ => None,
    }
}

/// Return an embedded system prompt string by mode name.
fn embedded_system_prompt(mode: &str) -> Option<&'static str> {
    match mode {
        "autopilot" => Some(include_str!("../system_prompts/autopilot.md")),
        "copilot" => Some(include_str!("../system_prompts/copilot.md")),
        "cruise" => Some(include_str!("../system_prompts/cruise.md")),
        _ => None,
    }
}

/// Load a preset by name from embedded data.
pub fn load_preset(name: &str) -> anyhow::Result<Preset> {
    let content = embedded_preset(name)
        .ok_or_else(|| anyhow::anyhow!("Unknown preset '{}'", name))?;
    let preset: Preset = toml::from_str(content)?;
    Ok(preset)
}

/// Load a prompt template by name from embedded data.
pub fn load_prompt(name: &str) -> anyhow::Result<String> {
    embedded_prompt(name)
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Unknown prompt template '{}'", name))
}

/// Load a system prompt by mode name from embedded data.
pub fn load_system_prompt(mode: &str) -> anyhow::Result<String> {
    embedded_system_prompt(mode)
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Unknown system prompt mode '{}'", mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn interpolate_replaces_variables() {
        let template = "Given task: {{task}}\nSections: {{sections}}";
        let mut vars = HashMap::new();
        vars.insert("task".to_string(), "Design a cache".to_string());
        vars.insert("sections".to_string(), "Architecture, API".to_string());
        let result = interpolate(template, &vars);
        assert_eq!(result, "Given task: Design a cache\nSections: Architecture, API");
    }

    #[test]
    fn interpolate_leaves_unknown_vars() {
        let template = "{{known}} and {{unknown}}";
        let mut vars = HashMap::new();
        vars.insert("known".to_string(), "hello".to_string());
        let result = interpolate(template, &vars);
        assert_eq!(result, "hello and {{unknown}}");
    }

    #[test]
    fn interpolate_escapes_injection_in_values() {
        let template = "Task: {{task}}\nSections: {{sections}}";
        let mut vars = HashMap::new();
        vars.insert("task".to_string(), "evil {{sections}} injection".to_string());
        vars.insert("sections".to_string(), "Architecture, API".to_string());
        let result = interpolate(template, &vars);
        // The {{sections}} in the user value should be escaped, not treated as a variable
        assert!(result.contains("evil { {sections}} injection"));
        assert!(result.contains("Sections: Architecture, API"));
    }

    #[test]
    fn load_preset_software() {
        let preset = load_preset("software").unwrap();
        assert_eq!(preset.name, "software");
        assert!(preset.dimensions.contains(&"Feasibility".to_string()));
        assert!(preset.sections.contains(&"Architecture".to_string()));
    }

    #[test]
    fn load_preset_general() {
        let preset = load_preset("general").unwrap();
        assert_eq!(preset.name, "general");
        assert!(preset.dimensions.contains(&"Completeness".to_string()));
    }

    #[test]
    fn load_prompt_template() {
        let content = load_prompt("brainstorm").unwrap();
        assert!(content.contains("{{task}}"));
    }

    #[test]
    fn load_system_prompt_autopilot() {
        let content = super::load_system_prompt("autopilot").unwrap();
        assert!(content.contains("pipeline"));
    }
}
