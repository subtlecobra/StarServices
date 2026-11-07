//! Pushes to the client, in order and without holding up the call that caused them.
//!
//! The topics are the client's (PushSubscriptions.h): `player/<geid>/social` for a player, and
//! `social/<group id>/#` for a group it joined, which it follows for everything but direct messages.
//! Contacts arrive under `account/<id>/#`. The bus delivers per account, so a group event is
//! published once for each member.

use prost::Message;
use sc_proto::internal::{push_publisher_client::PushPublisherClient, PublishRequest};
use tokio::sync::mpsc::{self, Sender};
use tonic::transport::Endpoint;
use tracing::warn;

pub const SOCIAL: &str = "sc.external.services.social.v1";
pub const CHAT: &str = "sc.external.services.chat.v1";
pub const CONTACTS: &str = "sc.external.services.contacts.v1";
pub const CUSTOM_LOBBY: &str = "sc.external.services.custom_lobby.v1";
pub const NOTIFICATIONS: &str = "sc.external.services.notifications.v1";
pub const PRESENCE: &str = "sc.external.services.presence.v1";
pub const RTC: &str = "sc.external.services.rtc.v1";
pub const SQUAD: &str = "sc.external.services.squad.v1";

const QUEUE: usize = 4096;

pub fn player_topic(geid: u64) -> String {
    format!("player/{geid}/social")
}

pub fn group_topic(group_id: &str) -> String {
    format!("social/{group_id}/events")
}

pub fn contacts_topic(account_id: u32) -> String {
    format!("account/{account_id}/contacts")
}

pub fn presence_topic(account_id: u32) -> String {
    format!("account/{account_id}/presence")
}

/// One message, ready to go to any number of accounts.
#[derive(Clone)]
pub struct Event {
    type_name: String,
    value: Vec<u8>,
}

impl Event {
    pub fn new(package: &str, name: &str, message: &impl Message) -> Self {
        Self { type_name: format!("{package}.{name}"), value: message.encode_to_vec() }
    }
}

pub struct Push {
    queue: Sender<PublishRequest>,
}

impl Push {
    /// Must run inside the runtime: it starts the task that publishes.
    pub fn connect(address: &str) -> Result<Self, String> {
        let endpoint = Endpoint::from_shared(address.to_string()).map_err(|e| format!("--push: {e}"))?;
        let mut client = PushPublisherClient::new(endpoint.connect_lazy());
        let (queue, mut pending) = mpsc::channel::<PublishRequest>(QUEUE);
        tokio::spawn(async move {
            while let Some(request) = pending.recv().await {
                if let Err(error) = client.publish(request).await {
                    warn!(%error, "push failed");
                }
            }
        });
        Ok(Self { queue })
    }

    /// Delivery is best effort: a client that was away reads the state back when it returns.
    pub fn send(&self, account_id: u32, topic: String, event: &Event) {
        let request = PublishRequest {
            account_id,
            topic,
            type_name: event.type_name.clone(),
            value: event.value.clone(),
            wait_for_subscriber_ms: 0,
        };
        if self.queue.try_send(request).is_err() {
            warn!("push queue full: message dropped");
        }
    }
}
