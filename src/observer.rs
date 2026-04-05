use std::sync::Mutex;

#[derive(Debug, Clone)]
pub enum BrainstormEvent {
    RoundStart { round: u32 },
    RoundComplete { round: u32, cost_usd: f64 },
    SectionConvergence { name: String, converged: bool, agreement: String },
    ConvergenceGuard { section: String, model: String, objection: String },
    SessionComplete { total_rounds: u32, total_cost: f64, converged: bool },
}

pub struct BrainstormObserver {
    event_log: Mutex<Vec<BrainstormEvent>>,
}

impl Default for BrainstormObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl BrainstormObserver {
    pub fn new() -> Self {
        Self {
            event_log: Mutex::new(Vec::new()),
        }
    }

    pub fn on_round_start(&self, round: u32) {
        self.event_log.lock().unwrap().push(BrainstormEvent::RoundStart { round });
    }

    pub fn on_round_complete(&self, round: u32, cost_usd: f64) {
        self.event_log.lock().unwrap().push(BrainstormEvent::RoundComplete { round, cost_usd });
    }

    pub fn on_section_convergence(&self, name: &str, converged: bool, agreement: &str) {
        self.event_log.lock().unwrap().push(BrainstormEvent::SectionConvergence {
            name: name.to_string(),
            converged,
            agreement: agreement.to_string(),
        });
    }

    pub fn on_convergence_guard(&self, section: &str, model: &str, objection: &str) {
        self.event_log.lock().unwrap().push(BrainstormEvent::ConvergenceGuard {
            section: section.to_string(),
            model: model.to_string(),
            objection: objection.to_string(),
        });
    }

    pub fn on_session_complete(&self, total_rounds: u32, total_cost: f64, converged: bool) {
        self.event_log.lock().unwrap().push(BrainstormEvent::SessionComplete {
            total_rounds,
            total_cost,
            converged,
        });
    }

    pub fn events(&self) -> Vec<BrainstormEvent> {
        self.event_log.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observer_records_round_events() {
        let observer = BrainstormObserver::new();
        observer.on_round_start(1);
        observer.on_round_complete(1, 0.05);
        let events = observer.events();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn observer_records_section_status() {
        let observer = BrainstormObserver::new();
        observer.on_section_convergence("Architecture", true, "3/3");
        let events = observer.events();
        assert_eq!(events.len(), 1);
    }
}
