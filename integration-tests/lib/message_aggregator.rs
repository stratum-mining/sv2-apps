use std::collections::VecDeque;
use stratum_apps::{
    stratum_core::parsers_sv2::{AnyMessageOwned, Tlv},
    sync::SharedLock,
};

use crate::types::MsgType;

#[allow(clippy::type_complexity)]
#[derive(Debug, Clone)]
pub struct MessagesAggregator {
    messages: SharedLock<VecDeque<(MsgType, AnyMessageOwned, Option<Vec<Tlv>>)>>,
}

impl Default for MessagesAggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl MessagesAggregator {
    /// Creates a new [`MessagesAggregator`].
    pub fn new() -> Self {
        Self {
            messages: SharedLock::new(VecDeque::new()),
        }
    }

    /// Adds a message to the end of the queue.
    pub fn add_message(&self, msg_type: MsgType, message: AnyMessageOwned) {
        self.add_message_with_tlvs(msg_type, message, None);
    }

    /// Adds a message with TLV fields to the end of the queue.
    pub fn add_message_with_tlvs(
        &self,
        msg_type: MsgType,
        message: AnyMessageOwned,
        tlv_fields: Option<Vec<Tlv>>,
    ) {
        self.messages
            .with(|messages| messages.push_back((msg_type, message, tlv_fields)))
            .unwrap();
    }

    /// Returns false if the queue is empty, true otherwise.
    pub fn is_empty(&self) -> bool {
        self.messages.with(|messages| messages.is_empty()).unwrap()
    }

    /// Clears all messages from the queue.
    pub fn clear(&self) {
        self.messages.with(|messages| messages.clear()).unwrap();
    }

    /// returns true if contains message_type
    pub fn has_message_type(&self, message_type: u8) -> bool {
        let has_message: bool = self
            .messages
            .with(|messages| {
                for (t, _, _) in messages.iter() {
                    if *t == message_type {
                        return true; // Exit early with `true`
                    }
                }
                false // Default value if no match is found
            })
            .unwrap();
        has_message
    }

    /// returns true if contains message_type and removes messages from the queue
    /// until the first message of type message_type.
    pub fn has_message_type_with_remove(&self, message_type: u8) -> bool {
        self.messages
            .with(|messages| {
                let mut cloned_messages = messages.clone();
                for (pos, (t, _, _)) in cloned_messages.iter().enumerate() {
                    if *t == message_type {
                        let drained = cloned_messages.drain(pos + 1..).collect();
                        *messages = drained;
                        return true;
                    }
                }
                false
            })
            .unwrap()
    }

    /// The aggregator queues messages in FIFO order, so this function returns the oldest message in
    /// the queue.
    ///
    /// The returned message is removed from the queue.
    pub fn next_message(&self) -> Option<(MsgType, AnyMessageOwned)> {
        self.next_message_with_tlvs()
            .map(|(msg_type, msg, _)| (msg_type, msg))
    }

    /// The aggregator queues messages in FIFO order, so this function returns the oldest message
    /// with its TLV fields in the queue.
    ///
    /// The returned message is removed from the queue.
    pub fn next_message_with_tlvs(&self) -> Option<(MsgType, AnyMessageOwned, Option<Vec<Tlv>>)> {
        self.messages
            .with(|messages| {
                let mut cloned = messages.clone();
                if let Some((msg_type, msg, tlv_fields)) = cloned.pop_front() {
                    *messages = cloned;
                    Some((msg_type, msg, tlv_fields))
                } else {
                    None
                }
            })
            .unwrap()
    }
}
