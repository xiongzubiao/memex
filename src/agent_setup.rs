use crate::observer::BrainstormObserver;
use std::sync::Arc;

/// Build the BrainstormObserver.
pub fn build_observer() -> Arc<BrainstormObserver> {
    Arc::new(BrainstormObserver::new())
}
