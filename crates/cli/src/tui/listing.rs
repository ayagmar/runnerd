//! The read behind the TUI's list of machines (ADR 0175): which opening of the list it belongs to, and whether its
//! answers are still wanted. Only the last opening's read is, and only while its list is open: closing the list stops
//! the read and its `ssh`, and opening it again gives up the read before, so a late answer from the first opening
//! never lands in the second. It reads nothing itself; the loop spawns the read it is handed.

use tokio::task::JoinHandle;

/// Which opening of the list an answer belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ticket(u64);

/// The read of the list of machines open now, if any. Nothing of it survives leaving the TUI or a switch of machine.
#[derive(Debug)]
pub(crate) struct Listing {
    /// The last ticket given out.
    asked: u64,
    state: ListRead,
}

/// A read under way, or none.
#[derive(Debug)]
enum ListRead {
    Idle,
    Reading {
        ticket: Ticket,
        /// The read; aborting it drops every machine's client, which stops its `ssh`.
        task: JoinHandle<()>,
    },
}

impl Listing {
    /// No read under way.
    pub(crate) fn new() -> Listing {
        Listing {
            asked: 0,
            state: ListRead::Idle,
        }
    }

    /// The list was opened: the read under way is given up and `read` starts another with a new ticket.
    pub(crate) fn open(&mut self, read: impl FnOnce(Ticket) -> JoinHandle<()>) {
        self.close();
        self.asked += 1;
        let ticket = Ticket(self.asked);
        self.state = ListRead::Reading {
            ticket,
            task: read(ticket),
        };
    }

    /// The list is not open: the read under way, if any, is given up.
    pub(crate) fn close(&mut self) {
        if let ListRead::Reading { task, .. } = std::mem::replace(&mut self.state, ListRead::Idle) {
            task.abort();
        }
    }

    /// Whether an answer of `ticket` is to be shown: only the read under way's is.
    pub(crate) fn takes(&self, ticket: Ticket) -> bool {
        matches!(self.state, ListRead::Reading { ticket: reading, .. } if reading == ticket)
    }
}

impl Drop for Listing {
    /// Leaving the TUI stops the read and its `ssh`, as closing the list does.
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::{Listing, Ticket};

    /// Set when the read holding it is dropped, as an aborted task drops it.
    struct Dropped(Arc<AtomicBool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Opens the list with a read that never answers by itself; returns its ticket and whether it was dropped.
    fn open(listing: &mut Listing) -> (Ticket, Arc<AtomicBool>) {
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = dropped.clone();
        let mut given = None;
        listing.open(|ticket| {
            given = Some(ticket);
            let held = Dropped(flag);
            tokio::spawn(async move {
                let _held = held;
                std::future::pending::<()>().await;
            })
        });
        (given.unwrap_or(Ticket(0)), dropped)
    }

    /// Catches: a list opened, closed and opened again taking the first opening's late answers, and that read's `ssh`
    /// left running behind the second.
    #[tokio::test]
    async fn opening_the_list_again_gives_up_the_first_read_and_its_answers() {
        let mut listing = Listing::new();
        let (first, first_dropped) = open(&mut listing);
        assert!(listing.takes(first), "the open list's answers are shown");
        listing.close();
        assert!(!listing.takes(first), "a closed list takes nothing");
        let (second, second_dropped) = open(&mut listing);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            first_dropped.load(Ordering::SeqCst),
            "the first read is stopped"
        );
        assert!(!listing.takes(first), "its late answer is dropped");
        assert!(
            listing.takes(second),
            "the second opening's answers are shown"
        );
        let (third, _) = open(&mut listing);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            second_dropped.load(Ordering::SeqCst),
            "reopening without a close stops the read before too"
        );
        assert!(!listing.takes(second), "dropped");
        assert!(listing.takes(third), "taken");
    }
}
