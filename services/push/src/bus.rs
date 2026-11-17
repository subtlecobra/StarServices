//! PushService.Listen fan-out.
//!
//! Topics are slash paths such as `account/1000001/login`, and subscriptions may use the MQTT-style
//! wildcards `+` (one level) and `#` (the rest), as seen from the real client. The client then
//! routes a delivered message by the envelope topic plus the Any's type URL.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use bytes::Bytes;
use tokio::sync::mpsc;
use tracing::warn;

use prost::Message;

use sc_proto::push::{
    push_command, push_control_message, push_message, ListenRequest, ListenResponse,
    PushControlMessage, PushEnvelope, PushEnvelopeType, PushMessage,
};

const TYPE_URL_PREFIX: &str = "type.googleapis.com/";

struct Listener {
    id: u64,
    account_id: Mutex<Option<u32>>,
    topics: Mutex<Vec<String>>,
    tx: mpsc::Sender<Bytes>,
}

impl Listener {
    /// An empty key is enough on a stream that already proved its account with a token.
    fn authorize(&self, key: &str, verify_key: impl Fn(&str) -> Option<u32>) -> bool {
        let mut account = self.account_id.lock().unwrap();
        if key.is_empty() {
            return account.is_some();
        }
        match (verify_key(key), *account) {
            (Some(owner), None) => {
                *account = Some(owner);
                true
            }
            (Some(owner), Some(stream)) => owner == stream,
            (None, _) => false,
        }
    }
}

#[derive(Default)]
pub struct PushBus {
    listeners: Mutex<Vec<Arc<Listener>>>,
    next_id: AtomicU64,
    next_envelope: AtomicU64,
}

fn response(envelope: PushEnvelope) -> Bytes {
    ListenResponse { envelopes: vec![envelope] }.encode_to_vec().into()
}

fn topic_matches(pattern: &str, topic: &str) -> bool {
    let mut levels = topic.split('/');
    for part in pattern.split('/') {
        if part == "#" {
            return true;
        }
        match levels.next() {
            Some(level) if part == "+" || part == level => {}
            _ => return false,
        }
    }
    levels.next().is_none()
}

impl PushBus {
    fn envelope(&self, topics: Vec<String>, data: push_message::Data) -> PushEnvelope {
        let id = self.next_envelope.fetch_add(1, Ordering::Relaxed) + 1;
        PushEnvelope {
            id: format!("env-{id}"),
            r#type: PushEnvelopeType::Message as i32,
            topics,
            messages: vec![PushMessage { data: Some(data) }],
            ..Default::default()
        }
    }

    fn control(&self, command: push_control_message::Command, subscribed: Vec<String>) -> Bytes {
        let event = (!subscribed.is_empty()).then(|| {
            push_control_message::Event::SubscriptionEvent(push_control_message::SubscriptionEvent {
                subscribed,
                unsubscribed: Vec::new(),
            })
        });
        let control = PushControlMessage { command: command as i32, event };
        response(self.envelope(Vec::new(), push_message::Data::Control(control)))
    }

    /// Registers a stream. The first message matters: the client reports the push service as
    /// unhealthy until a Listen response arrives.
    pub async fn register(&self, account_id: Option<u32>) -> (u64, mpsc::Receiver<Bytes>) {
        let (tx, rx) = mpsc::channel(256);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let opened = self.control(push_control_message::Command::StreamOpened, Vec::new());
        let _ = tx.send(opened).await;
        let listener = Listener { id, account_id: Mutex::new(account_id), topics: Mutex::new(Vec::new()), tx };
        self.listeners.lock().unwrap().push(Arc::new(listener));
        (id, rx)
    }

    pub fn unregister(&self, id: u64) {
        self.listeners.lock().unwrap().retain(|listener| listener.id != id);
    }

    /// Applies one ListenRequest. Returns the topics it subscribed. `verify_key` gives the account a
    /// subscription key belongs to. A key must match the stream, and a stream with no token gets its
    /// account from its first valid key.
    pub async fn handle_request(
        &self,
        id: u64,
        request: &[u8],
        verify_key: impl Fn(&str) -> Option<u32>,
    ) -> Vec<String> {
        let Some(listener) = self.listeners.lock().unwrap().iter().find(|l| l.id == id).cloned() else {
            return Vec::new();
        };
        let mut added = Vec::new();
        for command in ListenRequest::decode(request).map(|r| r.commands).unwrap_or_default() {
            match command.command {
                Some(push_command::Command::Subscribe(subscribe)) => {
                    if listener.authorize(&subscribe.key, &verify_key) {
                        added.extend(subscribe.topics);
                    } else {
                        warn!(stream = id, topics = ?subscribe.topics, "push subscribe refused");
                    }
                }
                Some(push_command::Command::Unsubscribe(unsubscribe)) => {
                    listener.topics.lock().unwrap().retain(|t| !unsubscribe.topics.contains(t));
                }
                _ => {} // AckCommand: delivery is best effort
            }
        }
        if !added.is_empty() {
            listener.topics.lock().unwrap().extend(added.iter().cloned());
            let event = self.control(push_control_message::Command::Event, added.clone());
            let _ = listener.tx.send(event).await;
        }
        added
    }

    fn subscribers(&self, account_id: u32, topic: &str) -> Vec<Arc<Listener>> {
        self.listeners
            .lock()
            .unwrap()
            .iter()
            .filter(|l| *l.account_id.lock().unwrap() == Some(account_id))
            .filter(|l| l.topics.lock().unwrap().iter().any(|pattern| topic_matches(pattern, topic)))
            .cloned()
            .collect()
    }

    /// Publishes to the account's subscribers of `topic`. Returns the number of streams reached.
    pub async fn publish(&self, account_id: u32, topic: &str, full_type: &str, value: Bytes) -> usize {
        let any = prost_types::Any {
            type_url: format!("{TYPE_URL_PREFIX}{full_type}"),
            value: value.to_vec(),
        };
        let mut delivered = 0;
        for listener in self.subscribers(account_id, topic) {
            let envelope = self.envelope(vec![topic.to_string()], push_message::Data::Any(any.clone()));
            if listener.tx.send(response(envelope)).await.is_ok() {
                delivered += 1;
            }
        }
        delivered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sc_proto::push::{PushCommand, SubscribeCommand};

    fn subscribe(topic: &str) -> Vec<u8> {
        let command = push_command::Command::Subscribe(SubscribeCommand {
            topics: vec![topic.to_string()],
            key: String::new(),
        });
        ListenRequest { commands: vec![PushCommand { command: Some(command) }] }.encode_to_vec()
    }

    fn verify(key: &str) -> Option<u32> {
        key.strip_prefix("key-").and_then(|account| account.parse().ok())
    }

    fn subscribe_with(topic: &str, key: &str) -> Vec<u8> {
        let command = push_command::Command::Subscribe(SubscribeCommand {
            topics: vec![topic.to_string()],
            key: key.to_string(),
        });
        ListenRequest { commands: vec![PushCommand { command: Some(command) }] }.encode_to_vec()
    }

    #[tokio::test]
    async fn a_stream_without_an_account_receives_nothing() {
        let bus = PushBus::default();
        let (stream, _rx) = bus.register(None).await;
        assert!(bus.handle_request(stream, &subscribe("account/1/login"), verify).await.is_empty());
        assert!(bus.handle_request(stream, &subscribe_with("account/1/login", "forged"), verify).await.is_empty());
        assert_eq!(bus.publish(1, "account/1/login", "t", Bytes::new()).await, 0);
    }

    #[tokio::test]
    async fn a_valid_key_gives_an_anonymous_stream_its_account_only() {
        let bus = PushBus::default();
        let (stream, _rx) = bus.register(None).await;
        assert_eq!(bus.handle_request(stream, &subscribe_with("account/+/login", "key-1"), verify).await.len(), 1);
        assert_eq!(bus.publish(1, "account/1/login", "t", Bytes::new()).await, 1);
        assert_eq!(bus.publish(2, "account/2/login", "t", Bytes::new()).await, 0);
    }

    #[tokio::test]
    async fn a_key_for_another_account_is_refused() {
        let bus = PushBus::default();
        let (stream, _rx) = bus.register(Some(1)).await;
        assert!(bus.handle_request(stream, &subscribe_with("account/2/#", "key-2"), verify).await.is_empty());
        assert_eq!(bus.handle_request(stream, &subscribe_with("account/1/#", "key-1"), verify).await.len(), 1);
    }

    #[test]
    fn wildcards_follow_the_subscriptions_the_client_sends() {
        assert!(topic_matches("account/1000001/login", "account/1000001/login"));
        assert!(topic_matches("account/1000001/#", "account/1000001/login"));
        assert!(topic_matches("account/+/presence", "account/7/presence"));
        assert!(!topic_matches("account/+/presence", "account/7/login"));
        assert!(!topic_matches("account/1000001/login", "account/1000002/login"));
        assert!(!topic_matches("account/1000001", "account/1000001/login"));
    }

    #[tokio::test]
    async fn delivers_only_to_the_matching_account_with_the_published_topic() {
        let bus = PushBus::default();
        let (mine, mut mine_rx) = bus.register(Some(1)).await;
        let (other, mut other_rx) = bus.register(Some(2)).await;
        mine_rx.recv().await.unwrap();
        other_rx.recv().await.unwrap();
        bus.handle_request(mine, &subscribe("account/1/login"), verify).await;
        bus.handle_request(other, &subscribe("account/+/login"), verify).await;
        mine_rx.recv().await.unwrap();
        other_rx.recv().await.unwrap();

        assert_eq!(bus.publish(1, "account/1/login", "sc.login.LoginCompleted", Bytes::new()).await, 1);
        let delivered = ListenResponse::decode(mine_rx.recv().await.unwrap()).unwrap();
        assert_eq!(delivered.envelopes[0].topics, ["account/1/login"]);
        assert!(other_rx.try_recv().is_err());
    }
}
