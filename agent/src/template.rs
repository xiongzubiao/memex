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
        assert_eq!(
            result,
            "Given task: Design a cache\nSections: Architecture, API"
        );
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
        vars.insert(
            "task".to_string(),
            "evil {{sections}} injection".to_string(),
        );
        vars.insert("sections".to_string(), "Architecture, API".to_string());
        let result = interpolate(template, &vars);
        // The {{sections}} in the user value should be escaped, not treated as a variable
        assert!(result.contains("evil { {sections}} injection"));
        assert!(result.contains("Sections: Architecture, API"));
    }

    #[test]
    fn load_preset_software() {
        let preset = crate::preset::get_preset("software").unwrap();
        assert_eq!(preset.name, "software");
        assert!(preset.dimensions.contains(&"Feasibility".to_string()));
        assert!(preset.sections.contains(&"Architecture".to_string()));
    }

    #[test]
    fn load_preset_general() {
        let preset = crate::preset::get_preset("general").unwrap();
        assert_eq!(preset.name, "general");
        assert!(preset.dimensions.contains(&"Completeness".to_string()));
    }
}
