//! Phase deadlines are activity evidence, never archive-completion evidence.

use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Inventory,
    Connect,
    Compare,
    Fetch,
    Drain,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct State {
    pub phase: Phase,
    pub deadline: Instant,
    pub rounds: usize,
    pub missing: usize,
    pub batches: usize,
    pub admitted: usize,
}

pub(super) struct Progress {
    tx: watch::Sender<State>,
    idle: Duration,
    compare: Duration,
}

impl Progress {
    pub fn new(idle: Duration, compare: Duration) -> Self {
        let (tx, _) = watch::channel(State {
            phase: Phase::Inventory,
            deadline: Instant::now() + idle,
            rounds: 0,
            missing: 0,
            batches: 0,
            admitted: 0,
        });
        Self { tx, idle, compare }
    }

    pub fn subscribe(&self) -> watch::Receiver<State> {
        self.tx.subscribe()
    }

    pub fn state(&self) -> State {
        *self.tx.borrow()
    }

    pub fn phase(&self, phase: Phase) {
        self.tx.send_modify(|s| {
            s.phase = phase;
            s.deadline = Instant::now()
                + match phase {
                    Phase::Connect => super::SETUP_TIME,
                    Phase::Compare => self.compare,
                    _ => self.idle,
                };
        });
    }

    // Called only after a matching, non-repeated response is accepted by the
    // reconciliation engine. Pings, foreign subscriptions and raw bytes do not count.
    pub fn round(&self, missing: usize) {
        self.tx.send_modify(|s| {
            s.rounds += 1;
            s.missing = missing;
            s.deadline = Instant::now() + self.compare;
        });
    }

    pub fn batch(&self) {
        self.tx.send_modify(|s| {
            s.batches += 1;
            s.deadline = Instant::now() + self.idle;
        });
    }

    pub fn admitted(&self) {
        self.tx.send_modify(|s| {
            s.admitted += 1;
            if matches!(s.phase, Phase::Fetch | Phase::Drain) {
                s.deadline = Instant::now() + self.idle;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn comparison_and_fetch_have_separate_progress_budgets() {
        let p = Progress::new(Duration::from_secs(120), Duration::from_secs(300));
        p.phase(Phase::Compare);
        let first = p.state().deadline;
        tokio::time::advance(Duration::from_secs(121)).await;
        assert!(Instant::now() < first);
        p.round(0); // Valid comparison with no missing events is still work.
        assert!(p.state().deadline > first);
        assert_eq!(p.state().rounds, 1);
        p.phase(Phase::Fetch);
        assert_eq!(
            p.state().deadline - Instant::now(),
            Duration::from_secs(120)
        );
        tokio::time::advance(Duration::from_secs(100)).await;
        p.admitted();
        assert_eq!(p.state().admitted, 1);
        assert_eq!(
            p.state().deadline - Instant::now(),
            Duration::from_secs(120)
        );
        p.batch();
        assert_eq!(p.state().batches, 1);
        tokio::time::advance(Duration::from_secs(121)).await;
        assert!(Instant::now() > p.state().deadline);
    }
}
