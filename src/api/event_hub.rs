//! The server's replay buffer for API events.
//!
//! Events are retained so a subscriber that polls periodically can pick up
//! everything that happened between its polls. The buffer is bounded, so a
//! subscriber that falls far enough behind cannot be served — and the important
//! property is that it is *told* so, rather than handed a shorter answer that
//! looks complete.

use std::collections::VecDeque;

#[derive(Clone, Default)]
pub struct EventHub {
    inner: std::sync::Arc<std::sync::Mutex<EventHubState>>,
}

#[derive(Default)]
struct EventHubState {
    next_sequence: u64,
    /// Retained events, oldest first.
    ///
    /// A deque rather than a `Vec` because eviction happens at the front on
    /// every push once the buffer is full, and draining the front of a `Vec`
    /// moves everything still retained.
    events: VecDeque<(u64, crate::api::schema::EventEnvelope)>,
}

/// What a subscriber gets back when it asks for what it missed.
#[derive(Debug, Clone, PartialEq)]
pub enum EventBatch {
    /// Everything matching between the requested sequence and `last_sequence`,
    /// up to the batch limit.
    Delivered {
        events: Vec<crate::api::schema::EventEnvelope>,
        /// Where the subscriber's cursor should now sit. Advances past events
        /// that did not match, so a subscriber filtering a rare kind does not
        /// rescan the whole buffer on every poll.
        last_sequence: u64,
    },
    /// Events the subscriber had not read were evicted before it asked.
    ///
    /// Reported rather than papered over: a shorter answer is indistinguishable
    /// from a quiet period, and a subscriber that cannot tell the difference
    /// silently builds an incomplete history.
    Gap {
        /// The oldest sequence still retained. Everything between the
        /// subscriber's cursor and this is gone.
        oldest_retained: u64,
        /// Where to resume from, having acknowledged the gap.
        resume_at: u64,
    },
}

impl EventHub {
    /// How many events are retained for replay.
    ///
    /// A subscriber further behind than this cannot be served what it missed,
    /// which is why falling off the end is reported rather than absorbed.
    pub const MAX_EVENTS: usize = 512;

    /// How many matching events one poll may take.
    ///
    /// A subscription used to emit at most one event per poll, and its
    /// connection sleeps between polls, so sustained matching traffic above
    /// that rate could never be delivered in full no matter how large the
    /// buffer was.
    pub const MAX_BATCH: usize = 64;

    pub fn push(&self, event: crate::api::schema::EventEnvelope) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        state.next_sequence += 1;
        let sequence = state.next_sequence;
        state.events.push_back((sequence, event));
        while state.events.len() > Self::MAX_EVENTS {
            state.events.pop_front();
        }
    }

    pub fn events_after(&self, sequence: u64) -> Vec<(u64, crate::api::schema::EventEnvelope)> {
        let Ok(state) = self.inner.lock() else {
            return Vec::new();
        };
        state
            .events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence)
            .cloned()
            .collect()
    }

    /// Takes up to [`Self::MAX_BATCH`] events of `kind` recorded after
    /// `sequence`, or reports that some were lost first.
    ///
    /// Scans under the lock and clones only what it returns, so a subscriber
    /// filtering a rare kind does not copy the whole retained tail on every
    /// poll in order to discard nearly all of it.
    pub fn matching_after(
        &self,
        sequence: u64,
        kind: crate::api::schema::EventKind,
    ) -> Option<EventBatch> {
        let Ok(state) = self.inner.lock() else {
            return None;
        };
        if sequence >= state.next_sequence {
            return None;
        }

        if let Some(gap) = state.gap_after(sequence) {
            return Some(gap);
        }

        let mut events = Vec::new();
        let mut last_sequence = sequence;
        for (event_sequence, event) in state
            .events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence)
        {
            if events.len() >= Self::MAX_BATCH {
                break;
            }
            last_sequence = *event_sequence;
            if event.event == kind {
                events.push(event.clone());
            }
        }

        (last_sequence > sequence).then_some(EventBatch::Delivered {
            events,
            last_sequence,
        })
    }

    pub fn current_sequence(&self) -> u64 {
        let Ok(state) = self.inner.lock() else {
            return 0;
        };
        state.next_sequence
    }
}

impl EventHubState {
    /// Whether anything after `sequence` was evicted before it could be read.
    fn gap_after(&self, sequence: u64) -> Option<EventBatch> {
        let oldest_retained = self.events.front().map(|(sequence, _)| *sequence)?;
        // The next event this subscriber wants is `sequence + 1`. If the buffer
        // no longer reaches back that far, the events in between are gone.
        (oldest_retained > sequence.saturating_add(1)).then_some(EventBatch::Gap {
            oldest_retained,
            resume_at: self.next_sequence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{EventData, EventEnvelope, EventKind};

    fn envelope(kind: EventKind) -> EventEnvelope {
        EventEnvelope {
            event: kind,
            data: EventData::WorkspaceClosed {
                workspace_id: "w1".to_string(),
                workspace: None,
            },
        }
    }

    fn push_many(hub: &EventHub, kind: EventKind, count: usize) {
        for _ in 0..count {
            hub.push(envelope(kind));
        }
    }

    #[test]
    fn nothing_new_reads_as_nothing() {
        let hub = EventHub::default();
        assert_eq!(hub.matching_after(0, EventKind::WorkspaceClosed), None);
    }

    #[test]
    fn a_batch_carries_many_events_rather_than_one() {
        // The throughput defect: one event per poll, and the connection sleeps
        // between polls, so sustained traffic could never be delivered whole.
        let hub = EventHub::default();
        push_many(&hub, EventKind::WorkspaceClosed, 10);

        let Some(EventBatch::Delivered {
            events,
            last_sequence,
        }) = hub.matching_after(0, EventKind::WorkspaceClosed)
        else {
            panic!("expected a delivered batch");
        };

        assert_eq!(events.len(), 10);
        assert_eq!(last_sequence, 10);
    }

    #[test]
    fn a_batch_is_capped_and_resumes_where_it_stopped() {
        let hub = EventHub::default();
        push_many(&hub, EventKind::WorkspaceClosed, EventHub::MAX_BATCH + 20);

        let Some(EventBatch::Delivered {
            events,
            last_sequence,
        }) = hub.matching_after(0, EventKind::WorkspaceClosed)
        else {
            panic!("expected a delivered batch");
        };
        assert_eq!(events.len(), EventHub::MAX_BATCH);

        let Some(EventBatch::Delivered { events, .. }) =
            hub.matching_after(last_sequence, EventKind::WorkspaceClosed)
        else {
            panic!("expected the rest");
        };
        assert_eq!(events.len(), 20, "the remainder arrives on the next poll");
    }

    #[test]
    fn the_cursor_advances_past_events_that_did_not_match() {
        // Otherwise a subscriber filtering a rare kind rescans the whole
        // retained buffer on every poll.
        let hub = EventHub::default();
        push_many(&hub, EventKind::WorkspaceClosed, 5);

        let Some(EventBatch::Delivered {
            events,
            last_sequence,
        }) = hub.matching_after(0, EventKind::WorkspaceCreated)
        else {
            panic!("expected an empty but advancing batch");
        };

        assert!(events.is_empty());
        assert_eq!(last_sequence, 5);
    }

    #[test]
    fn falling_behind_the_buffer_is_reported_rather_than_truncated() {
        // B2: the whole point. A subscriber that missed events must be able to
        // tell that from a quiet period.
        let hub = EventHub::default();
        push_many(&hub, EventKind::WorkspaceClosed, EventHub::MAX_EVENTS + 100);

        let Some(EventBatch::Gap {
            oldest_retained,
            resume_at,
        }) = hub.matching_after(0, EventKind::WorkspaceClosed)
        else {
            panic!("a subscriber this far behind must be told, not quietly served");
        };

        assert_eq!(oldest_retained, 101);
        assert_eq!(resume_at, EventHub::MAX_EVENTS as u64 + 100);
    }

    #[test]
    fn a_subscriber_at_the_oldest_retained_event_has_missed_nothing() {
        // The boundary: its next wanted event is exactly the oldest retained,
        // so this must not be reported as a gap.
        let hub = EventHub::default();
        push_many(&hub, EventKind::WorkspaceClosed, EventHub::MAX_EVENTS + 10);

        let oldest_retained = 11;
        let batch = hub.matching_after(oldest_retained - 1, EventKind::WorkspaceClosed);

        assert!(
            matches!(batch, Some(EventBatch::Delivered { .. })),
            "reachable history is not a gap"
        );
    }

    #[test]
    fn a_gap_resumes_at_the_present_and_does_not_repeat() {
        let hub = EventHub::default();
        push_many(&hub, EventKind::WorkspaceClosed, EventHub::MAX_EVENTS + 100);

        let Some(EventBatch::Gap { resume_at, .. }) =
            hub.matching_after(0, EventKind::WorkspaceClosed)
        else {
            panic!("expected a gap");
        };

        assert_eq!(
            hub.matching_after(resume_at, EventKind::WorkspaceClosed),
            None,
            "having resumed, the subscriber is current"
        );
    }

    #[test]
    fn the_buffer_stays_bounded() {
        let hub = EventHub::default();
        push_many(&hub, EventKind::WorkspaceClosed, EventHub::MAX_EVENTS * 3);

        assert_eq!(hub.events_after(0).len(), EventHub::MAX_EVENTS);
    }
}
