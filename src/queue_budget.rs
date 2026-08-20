//! Shared admission accounting for the server's bounded queues.
//!
//! Several queues in the server need the same guarantee — that a peer, client
//! or automation process which stops reading cannot make Herdr allocate without
//! limit — but they share no transport. The client writer is a `Mutex` and
//! `Condvar` over three lanes, the Windows PTY writer is a `std::sync::mpsc`
//! behind a Tokio channel, and the API ingress is a Tokio channel. What they
//! have in common is not a channel but a decision: *may this item be enqueued,
//! and what does the queue currently hold?*
//!
//! So this is deliberately not a queue. Callers keep their own storage and ask
//! a [`QueueBudget`] to admit each item, releasing it once it leaves. Counting
//! bytes as well as items matters because an item limit alone does not bound
//! memory: a single-slot lane holding one frame is bounded at one item and
//! still unbounded in bytes.

use std::fmt;

/// The point at which a queue refuses further items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueueLimits {
    pub(crate) max_items: usize,
    pub(crate) max_bytes: usize,
}

impl QueueLimits {
    pub(crate) const fn new(max_items: usize, max_bytes: usize) -> Self {
        Self {
            max_items,
            max_bytes,
        }
    }
}

/// Why an item was refused. Callers distinguish these because they mean
/// different things: a flood of small items is a different fault from one
/// oversized payload, and the operator response differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueOverflow {
    Items { limit: usize },
    Bytes { limit: usize },
}

impl fmt::Display for QueueOverflow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Items { limit } => write!(formatter, "queue item limit {limit} reached"),
            Self::Bytes { limit } => write!(formatter, "queue byte limit {limit} reached"),
        }
    }
}

/// Tracks what a queue currently holds against its limits.
///
/// Callers must release exactly the byte count they admitted for an item.
/// Nothing here can enforce that pairing, since the storage belongs to the
/// caller; releasing is expected to happen where the item is popped and its
/// size is still in hand.
#[derive(Debug)]
pub(crate) struct QueueBudget {
    limits: QueueLimits,
    items: usize,
    bytes: usize,
    peak_items: usize,
    peak_bytes: usize,
    rejected: u64,
}

impl QueueBudget {
    pub(crate) fn new(limits: QueueLimits) -> Self {
        Self {
            limits,
            items: 0,
            bytes: 0,
            peak_items: 0,
            peak_bytes: 0,
            rejected: 0,
        }
    }

    /// Accounts for one item of `bytes`, or refuses it.
    ///
    /// An item larger than the whole byte limit is admitted when the queue is
    /// empty. Refusing it instead would be a permanent stall rather than
    /// backpressure: nothing can drain to make room, so the lane would reject
    /// that item forever and never make progress. Admitting one oversized item
    /// at a time exceeds the limit briefly and stays bounded, which is the
    /// lesser problem.
    pub(crate) fn admit(&mut self, bytes: usize) -> Result<(), QueueOverflow> {
        if self.items >= self.limits.max_items {
            self.rejected = self.rejected.saturating_add(1);
            return Err(QueueOverflow::Items {
                limit: self.limits.max_items,
            });
        }
        let projected = self.bytes.saturating_add(bytes);
        if projected > self.limits.max_bytes && self.items > 0 {
            self.rejected = self.rejected.saturating_add(1);
            return Err(QueueOverflow::Bytes {
                limit: self.limits.max_bytes,
            });
        }

        self.items += 1;
        self.bytes = projected;
        self.peak_items = self.peak_items.max(self.items);
        self.peak_bytes = self.peak_bytes.max(self.bytes);
        Ok(())
    }

    /// Accounts for an item that must be enqueued regardless of the limits.
    ///
    /// For the rare message whose delivery matters more than the bound, such as
    /// the shutdown notice a client needs in order to close cleanly. It still
    /// records the item, because accounting that skips an enqueued item drifts
    /// out of step with the queue and makes every later release wrong.
    pub(crate) fn force_admit(&mut self, bytes: usize) {
        self.items += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        self.peak_items = self.peak_items.max(self.items);
        self.peak_bytes = self.peak_bytes.max(self.bytes);
    }

    /// Accounts for one item of `bytes` leaving the queue.
    pub(crate) fn release(&mut self, bytes: usize) {
        debug_assert!(
            self.items > 0 && bytes <= self.bytes,
            "released {bytes} bytes from a budget holding {} items / {} bytes; \
             admit and release are mispaired",
            self.items,
            self.bytes,
        );
        self.items = self.items.saturating_sub(1);
        self.bytes = self.bytes.saturating_sub(bytes);
    }

    /// Drops all accounting, for a lane whose contents were discarded wholesale.
    pub(crate) fn clear(&mut self) {
        self.items = 0;
        self.bytes = 0;
    }

    pub(crate) fn items(&self) -> usize {
        self.items
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn rejected(&self) -> u64 {
        self.rejected
    }

    pub(crate) fn peak_bytes(&self) -> usize {
        self.peak_bytes
    }

    /// Samples current depth into the opt-in profiler under fixed names.
    pub(crate) fn record(&self, items_gauge: &'static str, bytes_gauge: &'static str) {
        crate::render_prof::gauge(items_gauge, self.items as u64);
        crate::render_prof::gauge(bytes_gauge, self.bytes as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(max_items: usize, max_bytes: usize) -> QueueBudget {
        QueueBudget::new(QueueLimits::new(max_items, max_bytes))
    }

    #[test]
    fn admits_until_the_item_limit() {
        let mut budget = budget(2, 4_096);
        assert_eq!(budget.admit(1), Ok(()));
        assert_eq!(budget.admit(1), Ok(()));

        assert_eq!(budget.admit(1), Err(QueueOverflow::Items { limit: 2 }));
        assert_eq!(budget.items(), 2);
        assert_eq!(budget.rejected(), 1);
    }

    #[test]
    fn admits_until_the_byte_limit_even_with_item_room_left() {
        // The case an item-only bound misses entirely: two items, well under
        // the item limit, already past the memory the queue is allowed.
        let mut budget = budget(100, 1_000);
        assert_eq!(budget.admit(600), Ok(()));

        assert_eq!(
            budget.admit(600),
            Err(QueueOverflow::Bytes { limit: 1_000 })
        );
        assert_eq!(budget.items(), 1);
        assert_eq!(budget.bytes(), 600);
    }

    #[test]
    fn an_oversized_item_is_admitted_when_the_queue_is_empty() {
        // Refusing it would be a permanent stall, not backpressure — nothing
        // can drain to make room for it.
        let mut budget = budget(10, 1_000);

        assert_eq!(budget.admit(5_000), Ok(()));
        assert_eq!(budget.bytes(), 5_000);
        assert_eq!(budget.rejected(), 0);
    }

    #[test]
    fn an_oversized_item_waits_while_anything_else_is_queued() {
        let mut budget = budget(10, 1_000);
        assert_eq!(budget.admit(10), Ok(()));

        assert_eq!(
            budget.admit(5_000),
            Err(QueueOverflow::Bytes { limit: 1_000 })
        );

        // Once the lane drains it fits, so this is backpressure rather than a
        // dropped item.
        budget.release(10);
        assert_eq!(budget.admit(5_000), Ok(()));
    }

    #[test]
    fn releasing_restores_capacity() {
        let mut budget = budget(1, 1_000);
        assert_eq!(budget.admit(400), Ok(()));
        assert_eq!(budget.admit(400), Err(QueueOverflow::Items { limit: 1 }));

        budget.release(400);

        assert_eq!(budget.items(), 0);
        assert_eq!(budget.bytes(), 0);
        assert_eq!(budget.admit(400), Ok(()));
    }

    #[test]
    fn peaks_survive_the_queue_draining() {
        // What a queue holds right now says nothing about whether it was ever
        // in trouble; the peak is the diagnostic.
        let mut budget = budget(10, 10_000);
        let _ = budget.admit(4_000);
        let _ = budget.admit(3_000);
        budget.release(4_000);
        budget.release(3_000);

        assert_eq!(budget.bytes(), 0);
        assert_eq!(budget.peak_bytes(), 7_000);
    }

    #[test]
    fn clear_drops_current_accounting_but_keeps_the_peak() {
        let mut budget = budget(10, 10_000);
        let _ = budget.admit(4_000);

        budget.clear();

        assert_eq!(budget.items(), 0);
        assert_eq!(budget.bytes(), 0);
        assert_eq!(budget.peak_bytes(), 4_000);
    }

    #[test]
    fn force_admit_exceeds_the_limit_but_stays_accounted() {
        // The pairing is what matters: a forced item must still be released
        // exactly once, or the budget drifts and every later release is wrong.
        let mut budget = budget(1, 100);
        assert_eq!(budget.admit(100), Ok(()));

        budget.force_admit(50);
        assert_eq!(budget.items(), 2);
        assert_eq!(budget.bytes(), 150);

        budget.release(50);
        budget.release(100);
        assert_eq!(budget.items(), 0);
        assert_eq!(budget.bytes(), 0);
    }

    #[test]
    fn a_zero_byte_item_still_consumes_an_item_slot() {
        // Control messages can be empty framings; they must not be free, or a
        // flood of them escapes the bound.
        let mut budget = budget(2, 1_000);
        assert_eq!(budget.admit(0), Ok(()));
        assert_eq!(budget.admit(0), Ok(()));

        assert_eq!(budget.admit(0), Err(QueueOverflow::Items { limit: 2 }));
    }
}
