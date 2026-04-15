//! Brainstorm presets: predefined sections and evaluation dimensions per task type.
//!
//! Presets are embedded at compile time from `agent/presets/*.toml`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub dimensions: Vec<String>,
    pub sections: Vec<String>,
}

/// All shipped presets, embedded at compile time.
const PRESET_SOFTWARE: &str = include_str!("../presets/software.toml");
const PRESET_GENERAL: &str = include_str!("../presets/general.toml");
const PRESET_RESEARCH: &str = include_str!("../presets/research.toml");
const PRESET_ARTICLE: &str = include_str!("../presets/article.toml");
const PRESET_BOOK: &str = include_str!("../presets/book.toml");
const PRESET_STRATEGY: &str = include_str!("../presets/strategy.toml");

/// Load a preset by name. Returns `None` if the name is unknown.
pub fn get_preset(name: &str) -> Option<Preset> {
    let toml_str = match name {
        "software" => PRESET_SOFTWARE,
        "general" => PRESET_GENERAL,
        "research" => PRESET_RESEARCH,
        "article" => PRESET_ARTICLE,
        "book" => PRESET_BOOK,
        "strategy" => PRESET_STRATEGY,
        _ => return None,
    };
    toml::from_str(toml_str).ok()
}

/// List all available preset names.
pub fn available_presets() -> &'static [&'static str] {
    &[
        "software", "general", "research", "article", "book", "strategy",
    ]
}

/// Format a preset as context text to prepend to the brainstorm prompt.
pub fn format_preset_context(preset: &Preset) -> String {
    format!(
        "Task type: {}\nEvaluation dimensions: {}\nRequired sections: {}",
        preset.name,
        preset.dimensions.join(", "),
        preset.sections.join(", "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_presets_parse() {
        for name in available_presets() {
            let preset = get_preset(name);
            assert!(preset.is_some(), "preset {name} failed to parse");
            let p = preset.unwrap();
            assert_eq!(p.name, *name);
            assert!(!p.dimensions.is_empty());
            assert!(!p.sections.is_empty());
        }
    }

    #[test]
    fn unknown_preset_returns_none() {
        assert!(get_preset("nonexistent").is_none());
    }

    #[test]
    fn format_preset_context_includes_fields() {
        let preset = get_preset("software").unwrap();
        let ctx = format_preset_context(&preset);
        assert!(ctx.contains("software"));
        assert!(ctx.contains("Feasibility"));
        assert!(ctx.contains("Architecture"));
    }
}
