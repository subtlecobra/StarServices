//! sc.external.services.notifications.v1.NotificationsService. A notification is kept as its message,
//! with the columns that the calls select on beside it. Other services create notifications through
//! scbackend.internal.NotificationsInternal, which the gateway does not route to.

use std::{net::SocketAddr, process::ExitCode, sync::Arc};

use clap::Parser;
use prost::Message;
use sc_db::{internal, DatabaseArgs, Row};
use sc_api::internal::{ notifications_internal_server::{NotificationsInternal, NotificationsInternalServer}, CreateNotificationRequest, CreateNotificationResponse, };
use sc_proto::{
    common::api::v1::PaginationOrder,
    services::notifications::v1::{
    notifications_service_server::{NotificationsService, NotificationsServiceServer},
    FindRequest, FindResponse, Notification, NotificationRead, NotificationRequest, NotificationUnread, Recipient,
    ReadAllRequest, ReadAllResponse, ReadRequest, ReadResponse, RemoveAllRequest, RemoveAllResponse, RemoveRequest,
    RemoveResponse, UnreadRequest, UnreadResponse,
    },
};
use sc_runtime::{state::Account, CommonArgs, MAX_MESSAGE_BYTES};
use sc_social::{
    now_ms,
    page::{matches, Page},
    peers::PeerArgs,
    push::{Event, NOTIFICATIONS},
    transaction,
    tx::{Outbox, Queries},
    urn, Context,
};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS notifications (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
    geid BIGINT NOT NULL,
    body BYTEA NOT NULL,
    read BOOLEAN NOT NULL DEFAULT false,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS notifications_geid ON notifications (geid, created_at DESC);";

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
    /// Address of the push service.
    #[arg(long, env = "SCBACKEND_PUSH", default_value = "http://sc-push:50051")]
    push: String,
}

struct Notifications {
    ctx: Context,
}

fn notification(row: &Row) -> Result<Notification, Status> {
    let body: Vec<u8> = row.get("body");
    let mut notification = Notification::decode(body.as_slice()).map_err(internal)?;
    notification.id = row.get("id");
    notification.read = row.get("read");
    notification.created_at = (row.get::<_, i64>("created_at") / 1000) as u32;
    notification.updated_at = (row.get::<_, i64>("updated_at") / 1000) as u32;
    Ok(notification)
}

/// What one recipient of `request` gets. The table's columns fill in the rest when it is read.
fn addressed(request: &NotificationRequest, recipient: &Recipient, account: &Account) -> Notification {
    Notification {
        player_urn: urn::player(account.geid),
        facility: request.facility,
        r#type: request.r#type.clone(),
        category: request.category.clone(),
        title: request.title.clone(),
        message: request.message.clone(),
        icon: request.icon.clone(),
        priority: request.priority,
        recipient: Some(recipient.clone()),
        group_id: request.group_id.clone(),
        tag: request.tag.clone(),
        behaviors: request.behaviors.clone(),
        ..Default::default()
    }
}

fn topic(account: &Account) -> String {
    format!("account/{}/notifications", account.account_id)
}

fn field(notification: &Notification, name: &str) -> Option<String> {
    let text = |value: &String| Some(value.clone());
    match name {
        "id" => text(&notification.id),
        "type" => text(&notification.r#type),
        "category" => text(&notification.category),
        "title" => text(&notification.title),
        "group_id" => text(&notification.group_id),
        "tag" => text(&notification.tag),
        "facility" => Some(notification.facility.to_string()),
        "priority" => Some(notification.priority.to_string()),
        "read" => Some(notification.read.to_string()),
        "created_at" => Some(notification.created_at.to_string()),
        "updated_at" => Some(notification.updated_at.to_string()),
        _ => None,
    }
}

impl Notifications {
    fn notify(&self, account: &Account, event: &Event) {
        self.ctx.push.send(account.account_id, topic(account), event);
    }

    /// Marks these notifications, or all of them when `ids` is `None`, and returns what changed.
    async fn mark(&self, account: &Account, ids: Option<&[String]>, read: bool) -> Result<Vec<String>, Status> {
        let rows = self
            .ctx
            .db
            .query(
                "UPDATE notifications SET read = $2, updated_at = $3
                 WHERE geid = $1 AND read <> $2 AND ($4 OR id = ANY($5)) RETURNING id",
                &[&(account.geid as i64), &read, &now_ms(), &ids.is_none(), &ids.unwrap_or_default()],
            )
            .await
            .map_err(internal)?;
        Ok(rows.iter().map(|row| row.get("id")).collect())
    }
}

#[tonic::async_trait]
impl NotificationsService for Notifications {
    async fn remove(&self, request: Request<RemoveRequest>) -> Result<Response<RemoveResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let ids = request.into_inner().ids;
        self.ctx
            .db
            .execute("DELETE FROM notifications WHERE geid = $1 AND id = ANY($2)", &[&(account.geid as i64), &ids])
            .await
            .map_err(internal)?;
        Ok(Response::new(RemoveResponse {}))
    }

    /// Newest first unless the query sorts ascending.
    async fn find(&self, request: Request<FindRequest>) -> Result<Response<FindResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let query = request.into_inner().query.unwrap_or_default();
        let page = Page::new(query.pagination.as_ref())?;
        let rows = self
            .ctx
            .db
            .query(
                "SELECT id, body, read, created_at, updated_at FROM notifications
                 WHERE geid = $1 ORDER BY created_at DESC, id",
                &[&(account.geid as i64)],
            )
            .await
            .map_err(internal)?;
        let mut notifications = rows.iter().map(notification).collect::<Result<Vec<_>, _>>()?;
        notifications.retain(|n| matches(query.filter.as_ref(), &|name| field(n, name)));
        if query.sort.is_some_and(|sort| sort.order() == PaginationOrder::Ascending) {
            notifications.reverse();
        }
        let (results, page_info) = page.slice(notifications);
        Ok(Response::new(FindResponse { results, page_info: Some(page_info) }))
    }

    async fn read(&self, request: Request<ReadRequest>) -> Result<Response<ReadResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        for id in self.mark(&account, Some(&request.into_inner().ids), true).await? {
            self.notify(&account, &Event::new(NOTIFICATIONS, "NotificationRead", &NotificationRead { id }));
        }
        Ok(Response::new(ReadResponse {}))
    }

    async fn unread(&self, request: Request<UnreadRequest>) -> Result<Response<UnreadResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        for id in self.mark(&account, Some(&request.into_inner().ids), false).await? {
            self.notify(&account, &Event::new(NOTIFICATIONS, "NotificationUnread", &NotificationUnread { id }));
        }
        Ok(Response::new(UnreadResponse {}))
    }

    async fn read_all(&self, request: Request<ReadAllRequest>) -> Result<Response<ReadAllResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        for id in self.mark(&account, None, true).await? {
            self.notify(&account, &Event::new(NOTIFICATIONS, "NotificationRead", &NotificationRead { id }));
        }
        Ok(Response::new(ReadAllResponse {}))
    }

    async fn remove_all(&self, request: Request<RemoveAllRequest>) -> Result<Response<RemoveAllResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        self.ctx
            .db
            .execute("DELETE FROM notifications WHERE geid = $1", &[&(account.geid as i64)])
            .await
            .map_err(internal)?;
        Ok(Response::new(RemoveAllResponse {}))
    }
}

#[tonic::async_trait]
impl NotificationsInternal for Notifications {
    /// Every recipient gets it or none does. No account is checked: the caller is a backend service.
    async fn create(
        &self,
        request: Request<CreateNotificationRequest>,
    ) -> Result<Response<CreateNotificationResponse>, Status> {
        let ctx = &self.ctx;
        let request = request.into_inner().notification.ok_or_else(|| Status::invalid_argument("no notification"))?;
        if request.recipients.is_empty() {
            return Err(Status::invalid_argument("no recipients"));
        }
        let accounts = request
            .recipients
            .iter()
            .map(|recipient| ctx.directory.by_urn(&recipient.target_urn))
            .collect::<Result<Vec<_>, _>>()?;
        let ids = transaction!(ctx, tx, {
            let mut ids = Vec::with_capacity(accounts.len());
            for (recipient, account) in request.recipients.iter().zip(&accounts) {
                let now = now_ms();
                let row = tx
                    .query_opt(
                        "INSERT INTO notifications (geid, body, created_at, updated_at) VALUES ($1, $2, $3, $3)
                         RETURNING id, body, read, created_at, updated_at",
                        &[&(account.geid as i64), &addressed(&request, recipient, account).encode_to_vec(), &now],
                    )
                    .await?
                    .ok_or_else(|| Status::internal("notification not stored"))?;
                let stored = notification(&row)?;
                tx.send(account.account_id, topic(account), &Event::new(NOTIFICATIONS, "Notification", &stored));
                ids.push(stored.id);
            }
            Ok(ids)
        })?;
        Ok(Response::new(CreateNotificationResponse { ids }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Arc::new(Notifications { ctx: Context::open(&args.common, &args.database, &args.push, &PeerArgs::default(), SCHEMA).await? });
    let server = NotificationsServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    let internal = NotificationsInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, push = %args.push, "notifications listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_social::testing::TestDb;

    fn create_request(recipients: &[String]) -> Request<CreateNotificationRequest> {
        let recipients =
            recipients.iter().map(|urn| Recipient { target_urn: urn.clone(), ..Default::default() }).collect();
        let notification = NotificationRequest { title: "Hello".to_string(), recipients, ..Default::default() };
        Request::new(CreateNotificationRequest { notification: Some(notification) })
    }

    #[tokio::test]
    async fn a_created_notification_reaches_every_recipient_or_none() {
        let Some((db, ctx)) = TestDb::open(2, SCHEMA).await else { return };
        let service = Notifications { ctx };
        let players: Vec<String> = db.accounts.iter().map(|account| urn::player(account.geid)).collect();

        let ids = NotificationsInternal::create(&service, create_request(&players)).await.unwrap().into_inner().ids;
        assert_eq!(ids.len(), 2);

        let token = service.ctx.subscription_key(&db.accounts[1]);
        let mut find = Request::new(FindRequest::default());
        find.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
        let found = service.find(find).await.unwrap().into_inner().results;
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].id.as_str(), found[0].title.as_str()), (ids[1].as_str(), "Hello"));
        assert_eq!(found[0].player_urn, players[1]);
        assert!(!found[0].read);

        let unknown = [players[0].clone(), urn::player(1)];
        let refused = NotificationsInternal::create(&service, create_request(&unknown)).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::NotFound);
        let rows = service.ctx.query("SELECT id FROM notifications", &[]).await.unwrap();
        assert_eq!(rows.len(), 2);
        db.drop_schema().await;
    }

    #[tokio::test]
    async fn a_notification_needs_a_recipient() {
        let Some((db, ctx)) = TestDb::open(1, SCHEMA).await else { return };
        let service = Notifications { ctx };
        let refused = NotificationsInternal::create(&service, create_request(&[])).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::InvalidArgument);
        db.drop_schema().await;
    }

    #[test]
    fn a_request_is_addressed_to_each_recipient() {
        let account = Account { account_id: 7, handle: "Pilot".to_string(), geid: 300, created_at_ms: 0 };
        let recipient = Recipient { target_urn: urn::player(300), ..Default::default() };
        let request = NotificationRequest {
            title: "Delivery".to_string(),
            r#type: "delivery".to_string(),
            priority: 2,
            recipients: vec![recipient.clone()],
            ..Default::default()
        };
        let notification = addressed(&request, &recipient, &account);
        assert_eq!(notification.player_urn, urn::player(300));
        assert_eq!(notification.title, "Delivery");
        assert_eq!(notification.priority, 2);
        assert_eq!(notification.recipient, Some(recipient));
        assert!(!notification.read);
    }

    #[test]
    fn query_properties_read_the_notification() {
        let notification = Notification {
            id: "n".to_string(),
            r#type: "mission".to_string(),
            read: true,
            priority: 3,
            ..Default::default()
        };
        assert_eq!(field(&notification, "type").as_deref(), Some("mission"));
        assert_eq!(field(&notification, "read").as_deref(), Some("true"));
        assert_eq!(field(&notification, "priority").as_deref(), Some("3"));
        assert_eq!(field(&notification, "nothing"), None);
    }
}
