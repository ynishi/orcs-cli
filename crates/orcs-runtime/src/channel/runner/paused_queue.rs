//! Paused event queue for channel runners.
//!
//! Extracts common paused queue logic from ChannelRunner and ClientRunner.

use super::Event;
use std::collections::VecDeque;
use tracing::{debug, info, warn};

/// Maximum queue size for events received while paused.
pub const PAUSED_QUEUE_MAX_SIZE: usize = 128;

/// Queue for events received while a channel is paused.
///
/// This extracts common logic from ChannelRunner and ClientRunner
/// for handling events during the paused state.
#[derive(Debug, Default)]
pub struct PausedEventQueue {
    queue: VecDeque<Event>,
}

impl PausedEventQueue {
    /// Creates a new empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    /// Returns true if the queue is empty.
    #[must_use]
    #[allow(dead_code)] // Used in tests
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Returns the number of queued events.
    #[must_use]
    #[allow(dead_code)] // Used in tests
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Attempts to enqueue an event.
    ///
    /// Returns `true` if the event was queued, `false` if the queue is full.
    ///
    /// # Arguments
    ///
    /// * `event` - The event to enqueue
    /// * `runner_name` - Name for logging (e.g., "ChannelRunner", "ClientRunner")
    /// * `channel_id` - Channel ID for logging
    pub fn try_enqueue(
        &mut self,
        event: Event,
        runner_name: &str,
        channel_id: impl std::fmt::Display,
    ) -> bool {
        if self.queue.len() >= PAUSED_QUEUE_MAX_SIZE {
            warn!(
                "{} {}: paused queue full (max={}), dropping event",
                runner_name, channel_id, PAUSED_QUEUE_MAX_SIZE
            );
            return false;
        }

        debug!(
            "{} {}: queuing event while paused (queue_size={})",
            runner_name,
            channel_id,
            self.queue.len() + 1
        );
        self.queue.push_back(event);
        true
    }

    /// Drains all events from the queue.
    ///
    /// Returns an iterator over the queued events. Logs the drain count.
    ///
    /// # Arguments
    ///
    /// * `runner_name` - Name for logging
    /// * `channel_id` - Channel ID for logging
    pub fn drain(
        &mut self,
        runner_name: &str,
        channel_id: impl std::fmt::Display,
    ) -> impl Iterator<Item = Event> + '_ {
        let count = self.queue.len();
        if count > 0 {
            info!(
                "{} {}: draining {} queued events after resume",
                runner_name, channel_id, count
            );
        }
        self.queue.drain(..)
    }

    /// Pops the front event from the queue.
    #[allow(dead_code)] // Used in tests
    pub fn pop_front(&mut self) -> Option<Event> {
        self.queue.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_event::EventCategory;
    use orcs_types::ComponentId;

    fn test_event(op: &str) -> Event {
        Event {
            category: EventCategory::Echo,
            operation: op.to_string(),
            source: ComponentId::builtin("test"),
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn new_queue_is_empty() {
        let queue = PausedEventQueue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn enqueue_event() {
        let mut queue = PausedEventQueue::new();
        let event = test_event("test");

        let result = queue.try_enqueue(event, "TestRunner", "ch-1");

        assert!(result);
        assert_eq!(queue.len(), 1);
        assert!(!queue.is_empty());
    }

    #[test]
    fn enqueue_respects_max_size() {
        let mut queue = PausedEventQueue::new();

        // Fill to max
        for i in 0..PAUSED_QUEUE_MAX_SIZE {
            let event = test_event(&format!("event-{}", i));
            assert!(queue.try_enqueue(event, "TestRunner", "ch-1"));
        }

        // Next should fail
        let overflow_event = test_event("overflow");
        let result = queue.try_enqueue(overflow_event, "TestRunner", "ch-1");

        assert!(!result);
        assert_eq!(queue.len(), PAUSED_QUEUE_MAX_SIZE);
    }

    #[test]
    fn drain_empties_queue() {
        let mut queue = PausedEventQueue::new();

        queue.try_enqueue(test_event("a"), "TestRunner", "ch-1");
        queue.try_enqueue(test_event("b"), "TestRunner", "ch-1");
        queue.try_enqueue(test_event("c"), "TestRunner", "ch-1");

        let events: Vec<_> = queue.drain("TestRunner", "ch-1").collect();

        assert_eq!(events.len(), 3);
        assert!(queue.is_empty());
    }

    #[test]
    fn drain_preserves_order() {
        let mut queue = PausedEventQueue::new();

        queue.try_enqueue(test_event("first"), "TestRunner", "ch-1");
        queue.try_enqueue(test_event("second"), "TestRunner", "ch-1");
        queue.try_enqueue(test_event("third"), "TestRunner", "ch-1");

        let ops: Vec<_> = queue
            .drain("TestRunner", "ch-1")
            .map(|e| e.operation)
            .collect();

        assert_eq!(ops, vec!["first", "second", "third"]);
    }

    #[test]
    fn pop_front() {
        let mut queue = PausedEventQueue::new();

        queue.try_enqueue(test_event("first"), "TestRunner", "ch-1");
        queue.try_enqueue(test_event("second"), "TestRunner", "ch-1");

        let first = queue.pop_front().unwrap();
        assert_eq!(first.operation, "first");

        let second = queue.pop_front().unwrap();
        assert_eq!(second.operation, "second");

        assert!(queue.pop_front().is_none());
    }
}
