//! What a DGS has for a client of its own accord, by session: binds, unbinds, variable changes and remote method calls
//! of entities it runs, such as an elevator's carriage. The replicant subscribes to a session with `SessionUpdates` and
//! writes each update to the client as it comes. An update for a session nobody subscribed to is dropped: a client that
//! is not connected has nothing to be told.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use sc_api::internal::{session_update::Update, SessionUpdate};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tonic::Status;
use tracing::debug;

/// A client's session, as the replicant names it.
pub type SessionKey = (u64, u64);

pub type Sent = Result<SessionUpdate, Status>;

#[derive(Clone, Default)]
pub struct Outbox {
    sessions: Arc<Mutex<HashMap<SessionKey, UnboundedSender<Sent>>>>,
}

impl Outbox {
    /// The updates of `session` from now on. An earlier subscriber's stream ends.
    pub fn subscribe(&self, session: SessionKey) -> UnboundedReceiver<Sent> {
        let (sender, receiver) = unbounded_channel();
        self.sessions.lock().unwrap().insert(session, sender);
        receiver
    }

    /// Ends a session's stream, when its player leaves.
    pub fn close(&self, session: SessionKey) {
        self.sessions.lock().unwrap().remove(&session);
    }

    pub fn send(&self, session: SessionKey, update: Update) {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(sender) = sessions.get(&session) else {
            debug!(?session, "an update for a session with no subscriber, dropped");
            return;
        };
        if sender.send(Ok(SessionUpdate { update: Some(update) })).is_err() {
            sessions.remove(&session);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_api::internal::EntityUnbinds;

    fn unbind(id: u64) -> Update {
        Update::Unbinds(EntityUnbinds { entity_ids: vec![id] })
    }

    #[test]
    fn a_session_gets_its_updates_in_order_and_only_while_subscribed() {
        let outbox = Outbox::default();
        outbox.send((1, 7), unbind(1));
        let mut first = outbox.subscribe((1, 7));
        outbox.send((1, 7), unbind(2));
        outbox.send((1, 8), unbind(3));
        let mut second = outbox.subscribe((1, 7));
        outbox.send((1, 7), unbind(4));
        let received = |receiver: &mut UnboundedReceiver<Sent>| {
            let mut ids = Vec::new();
            while let Ok(Ok(update)) = receiver.try_recv() {
                let Some(Update::Unbinds(unbinds)) = update.update else { panic!() };
                ids.extend(unbinds.entity_ids);
            }
            ids
        };
        assert_eq!(received(&mut first), [2], "the later subscriber takes over");
        assert_eq!(received(&mut second), [4]);
        outbox.close((1, 7));
        outbox.send((1, 7), unbind(5));
        assert!(second.try_recv().is_err());
    }
}
