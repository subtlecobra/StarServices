//! Commands answer twice. The RPC reply only reports transport, and the client completes the command
//! from a CommandResponseBody pushed to the topics in its arguments. A failed RPC completes it too, so
//! only success needs the push.

use prost::Message;
use prost_types::Any;
use sc_proto::{
    common::command::v1::{notify_destination::Destination, CommandArguments, CommandResponseBody},
    google::rpc,
    internal::{push_publisher_client::PushPublisherClient, PublishRequest},
};
use tonic::{
    transport::{Channel, Endpoint},
    Status,
};
use tracing::warn;

const RESPONSE_BODY: &str = "sc.external.common.command.v1.CommandResponseBody";
const TYPE_URL_PREFIX: &str = "type.googleapis.com/";
const SUBSCRIBE_WAIT_MS: u32 = 1_000;

#[derive(Clone)]
pub struct Push {
    client: PushPublisherClient<Channel>,
}

/// The id of a command, which every command needs.
pub fn command_id(arguments: Option<&CommandArguments>) -> Result<&str, Status> {
    arguments
        .map(|arguments| arguments.id.as_str())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Status::invalid_argument("missing command id"))
}

fn topics(arguments: &CommandArguments) -> impl Iterator<Item = &String> {
    arguments
        .destinations
        .iter()
        .filter_map(|destination| match &destination.destination {
            Some(Destination::Push(push)) => Some(&push.topics),
            _ => None,
        })
        .flatten()
}

fn response_body(id: &str, response_type: &str, response: &impl Message) -> CommandResponseBody {
    CommandResponseBody {
        id: id.to_string(),
        status: Some(rpc::Status::default()),
        context: Some(Any {
            type_url: format!("{TYPE_URL_PREFIX}{response_type}"),
            value: response.encode_to_vec(),
        }),
    }
}

impl Push {
    pub fn connect(address: &str) -> Result<Self, String> {
        let endpoint = Endpoint::from_shared(address.to_string()).map_err(|e| format!("push address: {e}"))?;
        Ok(Self { client: PushPublisherClient::new(endpoint.connect_lazy()) })
    }

    /// Reports a committed command. The work is done by now, so a push failure is logged and not
    /// returned: the client would otherwise see a committed command as failed.
    pub async fn complete(
        &self,
        account_id: u32,
        arguments: &CommandArguments,
        response_type: &str,
        response: &impl Message,
    ) {
        let value = response_body(&arguments.id, response_type, response).encode_to_vec();
        for topic in topics(arguments) {
            let request = PublishRequest {
                account_id,
                topic: topic.clone(),
                type_name: RESPONSE_BODY.to_string(),
                value: value.clone(),
                wait_for_subscriber_ms: SUBSCRIBE_WAIT_MS,
            };
            if let Err(error) = self.client.clone().publish(request).await {
                warn!(%error, command = %arguments.id, "command result not pushed");
            }
        }
    }

    /// Sends a message to whoever is subscribed right now.
    pub async fn notify(&self, account_id: u32, topic: String, type_name: &str, message: &impl Message) {
        let request = PublishRequest {
            account_id,
            topic,
            type_name: type_name.to_string(),
            value: message.encode_to_vec(),
            wait_for_subscriber_ms: 0,
        };
        if let Err(error) = self.client.clone().publish(request).await {
            warn!(%error, "notification not pushed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sc_proto::{
        common::command::v1::{KafkaDestination, NotifyDestination},
        services::{entitlement::v2::UpdateArenaCommanderLoadoutResponse, push::v1::PushDestination},
    };

    fn arguments() -> CommandArguments {
        let push = PushDestination {
            addresses: vec!["address:player_id:1".into()],
            topics: vec!["commands/s/global/entitlement/requests/x/id".into()],
        };
        let kafka = KafkaDestination { topic: "ignored".into() };
        CommandArguments {
            id: "id".into(),
            destinations: vec![
                NotifyDestination { destination: Some(Destination::Kafka(kafka)) },
                NotifyDestination { destination: Some(Destination::Push(push)) },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn only_push_topics_are_published() {
        let arguments = arguments();
        let found: Vec<_> = topics(&arguments).collect();
        assert_eq!(found, ["commands/s/global/entitlement/requests/x/id"]);
    }

    #[test]
    fn a_command_needs_an_id() {
        assert_eq!(command_id(Some(&arguments())).unwrap(), "id");
        assert!(command_id(Some(&CommandArguments::default())).is_err());
        assert!(command_id(None).is_err());
    }

    #[test]
    fn the_response_travels_in_the_context() {
        let body = response_body("id", "a.B", &UpdateArenaCommanderLoadoutResponse::default());
        assert_eq!(body.id, "id");
        assert_eq!(body.status.unwrap().code, 0);
        assert_eq!(body.context.unwrap().type_url, "type.googleapis.com/a.B");
    }
}
