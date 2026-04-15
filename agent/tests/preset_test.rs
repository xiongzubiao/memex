use memex_agent::preset::get_preset;

#[test]
fn load_all_presets() {
    let names = [
        "software", "general", "research", "article", "book", "strategy",
    ];
    for name in &names {
        let preset = get_preset(name).unwrap_or_else(|| panic!("Failed to load preset '{name}'"));
        assert!(!preset.name.is_empty(), "Preset '{name}' has empty name");
        assert!(
            !preset.dimensions.is_empty(),
            "Preset '{name}' has no dimensions",
        );
        assert!(
            !preset.sections.is_empty(),
            "Preset '{name}' has no sections",
        );
    }
}

#[test]
fn research_preset_has_scientific_dimensions() {
    let preset = get_preset("research").unwrap();
    assert_eq!(preset.name, "research");
    assert!(preset.dimensions.contains(&"Novelty".to_string()));
    assert!(preset.dimensions.contains(&"Rigor".to_string()));
    assert!(preset.dimensions.contains(&"Reproducibility".to_string()));
}

#[test]
fn article_preset_has_writing_dimensions() {
    let preset = get_preset("article").unwrap();
    assert_eq!(preset.name, "article");
    assert!(preset.dimensions.contains(&"Clarity".to_string()));
    assert!(preset.dimensions.contains(&"Argument Strength".to_string()));
}

#[test]
fn book_preset_has_narrative_dimensions() {
    let preset = get_preset("book").unwrap();
    assert_eq!(preset.name, "book");
    assert!(preset.dimensions.contains(&"Narrative Arc".to_string()));
    assert!(preset.dimensions.contains(&"Pacing".to_string()));
}

#[test]
fn strategy_preset_has_business_dimensions() {
    let preset = get_preset("strategy").unwrap();
    assert_eq!(preset.name, "strategy");
    assert!(preset.dimensions.contains(&"Market Fit".to_string()));
    assert!(preset.dimensions.contains(&"ROI".to_string()));
}
