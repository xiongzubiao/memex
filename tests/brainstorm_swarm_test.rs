use brainstormer::tools::brainstorm_swarm::*;

#[test]
fn t13_degraded_mode_m_gte_3_one_failure() {
    let results = vec![
        Ok("Output from model 1".to_string()),
        Err("Timeout".to_string()),
        Ok("Output from model 3".to_string()),
    ];
    let model_count = 3;
    let (outputs, warning) = handle_partial_results(results, model_count);
    assert_eq!(outputs.len(), 2);
    assert!(warning.is_some());
    assert!(warning.unwrap().contains("1 model failed"));
}

#[test]
fn t14_degraded_mode_m_gte_3_one_refusal() {
    let results = vec![
        Ok("Output from model 1".to_string()),
        Ok("[Refused] I cannot assist with that".to_string()),
        Ok("Output from model 3".to_string()),
    ];
    let model_count = 3;
    let (outputs, warning) = handle_partial_results(results, model_count);
    assert_eq!(outputs.len(), 2);
    assert!(warning.is_some());
}

#[test]
fn t14b_degraded_mode_m_eq_2_one_failure() {
    let results = vec![
        Ok("Output from model 1".to_string()),
        Err("Timeout".to_string()),
    ];
    let model_count = 2;
    let (outputs, warning) = handle_partial_results(results, model_count);
    assert_eq!(outputs.len(), 1);
    assert!(warning.is_some());
    assert!(warning.unwrap().contains("pipeline paused"));
}
