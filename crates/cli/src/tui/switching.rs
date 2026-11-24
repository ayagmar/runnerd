//! Which machine the TUI reads, and the switch to another under way (ADR 0166). Decides which switch's answer is
//! taken: only the last one asked for, so a slow machine chosen first cannot land over a faster one chosen after.
//! It starts nothing and reads nothing itself; the loop spawns the read it is handed and swaps the screen.

use tokio::task::JoinHandle;

/// Which switch an answer belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ticket(u64);

/// The machine read now and the one being read instead, if any. Nothing of it survives leaving the TUI.
#[derive(Debug)]
pub(crate) struct Switching {
    /// The machine read now: its name in `machines.toml`, or `None` for this machine's daemon.
    current: Option<String>,
    /// The last ticket given out.
    asked: u64,
    /// The switch under way, if one is.
    pending: Pending,
}

/// A switch under way, or none.
#[derive(Debug)]
enum Pending {
    None,
    Reading {
        ticket: Ticket,
        target: Option<String>,
        /// The read; aborting it drops its client, which stops its `ssh`.
        task: JoinHandle<()>,
    },
}

impl Switching {
    /// Reading `current` with no switch under way.
    pub(crate) fn new(current: Option<String>) -> Switching {
        Switching {
            current,
            asked: 0,
            pending: Pending::None,
        }
    }

    /// The machine read now.
    pub(crate) fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// The person chose `target`: the switch under way is given up, and unless `target` is the machine read now,
    /// `read` starts reading it with a new ticket.
    pub(crate) fn choose(
        &mut self,
        target: Option<String>,
        read: impl FnOnce(Ticket) -> JoinHandle<()>,
    ) {
        if let Pending::Reading { task, .. } = std::mem::replace(&mut self.pending, Pending::None) {
            task.abort();
        }
        if target == self.current {
            return;
        }
        self.asked += 1;
        let ticket = Ticket(self.asked);
        self.pending = Pending::Reading {
            ticket,
            target,
            task: read(ticket),
        };
    }

    /// The read of `ticket` answered. Whether its answer is to be taken: only the switch under way's is, and then
    /// its machine is the one read now. A failed one, taken, leaves the machine read now as it was.
    pub(crate) fn answered(&mut self, ticket: Ticket, succeeded: bool) -> bool {
        match &self.pending {
            Pending::Reading {
                ticket: pending, ..
            } if *pending == ticket => {}
            _ => return false,
        }
        if let Pending::Reading { target, .. } = std::mem::replace(&mut self.pending, Pending::None)
            && succeeded
        {
            self.current = target;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::{Switching, Ticket};

    /// Set when the read holding it is dropped, as an aborted task drops it.
    struct Dropped(Arc<AtomicBool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Chooses `target`, with a read that never answers by itself; returns its ticket and whether it was dropped.
    fn choose(
        switching: &mut Switching,
        target: Option<&str>,
    ) -> (Option<Ticket>, Arc<AtomicBool>) {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut given = None;
        let flag = dropped.clone();
        switching.choose(target.map(str::to_owned), |ticket| {
            given = Some(ticket);
            let held = Dropped(flag);
            tokio::spawn(async move {
                let _held = held;
                std::future::pending::<()>().await;
            })
        });
        (given, dropped)
    }

    /// Catches: a slow machine chosen first landing over a faster one chosen after it, and its `ssh` left running.
    #[tokio::test]
    async fn only_the_last_machine_chosen_is_switched_to() {
        let mut switching = Switching::new(None);
        let (dory, dory_dropped) = choose(&mut switching, Some("dory"));
        let (skiff, _) = choose(&mut switching, Some("skiff"));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            dory_dropped.load(Ordering::SeqCst),
            "dory's read is given up"
        );
        assert!(
            switching.answered(skiff.unwrap(), true),
            "skiff's answer is taken"
        );
        assert_eq!(switching.current(), Some("skiff"));
        assert!(
            !switching.answered(dory.unwrap(), true),
            "dory's late answer is dropped"
        );
        assert_eq!(switching.current(), Some("skiff"));
    }

    /// Catches: going back to the machine on screen doing nothing while a switch to another still lands.
    #[tokio::test]
    async fn choosing_the_machine_read_now_gives_up_the_switch_under_way() {
        let mut switching = Switching::new(None);
        let (skiff, dropped) = choose(&mut switching, Some("skiff"));
        let (again, _) = choose(&mut switching, None);
        assert_eq!(again, None, "nothing to read");
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(dropped.load(Ordering::SeqCst), "skiff's read is given up");
        assert!(!switching.answered(skiff.unwrap(), true), "dropped");
        assert_eq!(switching.current(), None);
    }

    /// Catches: a failed switch changing which machine the TUI says it reads.
    #[tokio::test]
    async fn a_failed_switch_keeps_the_machine_read_now() {
        let mut switching = Switching::new(Some("skiff".to_owned()));
        let (dory, _) = choose(&mut switching, Some("dory"));
        assert!(
            switching.answered(dory.unwrap(), false),
            "its failure is shown"
        );
        assert_eq!(switching.current(), Some("skiff"));
    }
}
