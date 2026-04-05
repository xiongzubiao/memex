use brainstormer::tools::brainstorm_swarm::*;

#[test]
fn retry_delay_increases_exponentially() {
    let d1 = retry_delay(1);
    let d2 = retry_delay(2);
    let d3 = retry_delay(3);
    assert!(d2 > d1);
    assert!(d3 > d2);
    assert!(d1.as_millis() >= 500 && d1.as_millis() <= 1500);
    assert!(d3.as_millis() >= 2000);
}

#[test]
fn retry_delay_capped_at_max() {
    let d10 = retry_delay(10);
    assert!(d10.as_secs() <= 30);
}
