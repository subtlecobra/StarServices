//! sc.external.services.chat.v1.ChatService. Messages stay in the table after erasure, without their
//! content, so a page still shows that something was there.
//!
//! sc-chat owns `chat_messages`. Groups are sc-social's, which it asks through SocialInternal; when
//! sc-social destroys a group it has the group's messages deleted through
//! scbackend.internal.GroupDataInternal, served on the same port.

use std::{net::SocketAddr, process::ExitCode, sync::Arc};

use clap::Parser;
use sc_db::{internal, DatabaseArgs, Row};
use sc_proto::{
    internal::{
        group_data_internal_server::{GroupDataInternal, GroupDataInternalServer},
        DeleteGroupDataRequest, DeleteGroupDataResponse,
    },
    services::chat::v1::{
        chat_service_server::{ChatService, ChatServiceServer},
        EditMessageRequest, EditMessageResponse, EraseMessageRequest, EraseMessageResponse, ListMessagesRequest,
        ListMessagesResponse, Message, MessageEdited, MessageErased, MessageReceived, SendMessageRequest,
        SendMessageResponse,
    },
};
use sc_runtime::{CommonArgs, MAX_MESSAGE_BYTES};
use sc_social::{
    groups::{self, GroupView},
    now_ms,
    page::Page,
    peers::PeerArgs,
    push::{Event, CHAT},
    urn, Context,
};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

/// sc-chat's table, which it alone reads and writes.
const TABLES: &str = "
CREATE TABLE IF NOT EXISTS chat_messages (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
    group_id TEXT NOT NULL,
    sender_geid BIGINT NOT NULL,
    account_id BIGINT NOT NULL,
    nickname TEXT NOT NULL,
    content TEXT NOT NULL,
    kind INT NOT NULL,
    shard_id TEXT NOT NULL DEFAULT '',
    build_cl TEXT NOT NULL DEFAULT '',
    created_at BIGINT NOT NULL,
    edited_at BIGINT NOT NULL DEFAULT 0,
    erased_at BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS chat_messages_group ON chat_messages (group_id, created_at DESC);";

const MAX_CONTENT_BYTES: usize = 4096;
const COLUMNS: &str = "id, group_id, sender_geid, account_id, nickname, content, kind, created_at, edited_at, erased_at";

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
    #[command(flatten)]
    peers: PeerArgs,
}

struct Chat {
    ctx: Context,
}

fn message(row: &Row) -> Message {
    let nickname: String = row.get("nickname");
    Message {
        id: row.get("id"),
        group_id: row.get("group_id"),
        sender_urn: urn::player(row.get::<_, i64>("sender_geid") as u64),
        sender_display_name: nickname.clone(),
        sender_nickname: nickname,
        content: row.get("content"),
        created_at: row.get("created_at"),
        erased_at: row.get("erased_at"),
        edited_at: row.get("edited_at"),
        account_id: row.get::<_, i64>("account_id") as u32,
        group_type: row.get("kind"),
    }
}

fn check_content(content: &str) -> Result<(), Status> {
    if content.trim().is_empty() {
        return Err(Status::invalid_argument("empty message"));
    }
    if content.len() > MAX_CONTENT_BYTES {
        return Err(Status::invalid_argument("message too long"));
    }
    Ok(())
}

impl Chat {
    async fn find(&self, id: &str) -> Result<Message, Status> {
        let row = self
            .ctx
            .db
            .query_opt(&format!("SELECT {COLUMNS} FROM chat_messages WHERE id = $1"), &[&id])
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no message {id}")))?;
        Ok(message(&row))
    }

    fn publish(&self, view: &GroupView, event: &Event) {
        groups::to_group(&self.ctx, &view.group, &view.members, event);
    }

    async fn delete_groups(&self, ids: &[String]) -> Result<u64, Status> {
        self.ctx.db.execute("DELETE FROM chat_messages WHERE group_id = ANY($1)", &[&ids]).await.map_err(internal)
    }
}

#[tonic::async_trait]
impl ChatService for Chat {
    async fn send_message(
        &self,
        request: Request<SendMessageRequest>,
    ) -> Result<Response<SendMessageResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        check_content(&request.content)?;
        let view = ctx.peers.member_of(&request.group_id, account.geid).await?;
        let group = &view.group;
        if !group.settings.as_ref().and_then(|s| s.chat.as_ref()).is_some_and(|chat| chat.enabled) {
            return Err(Status::failed_precondition("this group has no chat"));
        }
        let metadata = request.metadata.unwrap_or_default();
        let row = ctx
            .db
            .query_opt(
                &format!(
                    "INSERT INTO chat_messages
                         (group_id, sender_geid, account_id, nickname, content, kind, shard_id, build_cl, created_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING {COLUMNS}"
                ),
                &[
                    &group.id,
                    &(account.geid as i64),
                    &i64::from(account.account_id),
                    &account.handle,
                    &request.content,
                    &group.r#type,
                    &metadata.shard_id,
                    &metadata.build_cl,
                    &now_ms(),
                ],
            )
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::internal("message not stored"))?;
        // A group destroyed since the check above has had its messages deleted already.
        if ctx.peers.gone(&group.id).await {
            self.delete_groups(std::slice::from_ref(&group.id)).await?;
            return Err(Status::not_found(format!("no group {}", group.id)));
        }
        let received = MessageReceived { message: Some(message(&row)) };
        self.publish(&view, &Event::new(CHAT, "MessageReceived", &received));
        Ok(Response::new(SendMessageResponse {}))
    }

    /// The sender and whoever runs the group may erase.
    async fn erase_message(
        &self,
        request: Request<EraseMessageRequest>,
    ) -> Result<Response<EraseMessageResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let found = self.find(&request.into_inner().message_id).await?;
        let view = ctx.peers.member_of(&found.group_id, account.geid).await?;
        if found.sender_urn != urn::player(account.geid) && !groups::can_manage(&view.group, account.geid) {
            return Err(Status::permission_denied("not allowed to erase this message"));
        }
        let erased_at = now_ms();
        ctx.db
            .execute("UPDATE chat_messages SET content = '', erased_at = $2 WHERE id = $1", &[&found.id, &erased_at])
            .await
            .map_err(internal)?;
        let erased = MessageErased { message_id: found.id, group_id: view.group.id.clone(), erased_at };
        self.publish(&view, &Event::new(CHAT, "MessageErased", &erased));
        Ok(Response::new(EraseMessageResponse {}))
    }

    async fn edit_message(
        &self,
        request: Request<EditMessageRequest>,
    ) -> Result<Response<EditMessageResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        check_content(&request.content)?;
        let found = self.find(&request.message_id).await?;
        if found.sender_urn != urn::player(account.geid) {
            return Err(Status::permission_denied("only the sender can edit a message"));
        }
        if found.erased_at != 0 {
            return Err(Status::failed_precondition("this message was erased"));
        }
        let view = ctx.peers.member_of(&found.group_id, account.geid).await?;
        let edited_at = now_ms();
        ctx.db
            .execute(
                "UPDATE chat_messages SET content = $2, edited_at = $3 WHERE id = $1",
                &[&found.id, &request.content, &edited_at],
            )
            .await
            .map_err(internal)?;
        let edited =
            MessageEdited { message_id: found.id, group_id: view.group.id.clone(), content: request.content, edited_at };
        self.publish(&view, &Event::new(CHAT, "MessageEdited", &edited));
        Ok(Response::new(EditMessageResponse {}))
    }

    /// Newest first, so that the next page reaches further back.
    async fn list_messages(
        &self,
        request: Request<ListMessagesRequest>,
    ) -> Result<Response<ListMessagesResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let page = Page::new(request.pagination.as_ref())?;
        ctx.peers.member_of(&request.group_id, account.geid).await?;
        let rows = ctx
            .db
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM chat_messages WHERE group_id = $1
                     ORDER BY created_at DESC, id LIMIT $2 OFFSET $3"
                ),
                &[&request.group_id, &page.fetch(), &page.offset],
            )
            .await
            .map_err(internal)?;
        let (rows, page_info) = page.finish(rows);
        let messages = rows.iter().map(message).collect();
        Ok(Response::new(ListMessagesResponse { messages, page_info: Some(page_info) }))
    }
}

/// sc-social, which destroyed these groups, has their messages deleted.
#[tonic::async_trait]
impl GroupDataInternal for Chat {
    async fn delete_group_data(
        &self,
        request: Request<DeleteGroupDataRequest>,
    ) -> Result<Response<DeleteGroupDataResponse>, Status> {
        let deleted = self.delete_groups(&request.into_inner().group_ids).await?;
        Ok(Response::new(DeleteGroupDataResponse { deleted }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let ctx = Context::open(&args.common, &args.database, &args.push, &args.peers, TABLES).await?;
    let service = Arc::new(Chat { ctx });
    let server = ChatServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    let internal = GroupDataInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, push = %args.push, "chat listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use sc_proto::services::social::v1::GroupType;
    use sc_social::testing::{nowhere, serve, SocialProcess, TestDb};
    use tonic::{service::Routes, Code};

    fn signed<T>(message: T, token: &str) -> Request<T> {
        let mut request = Request::new(message);
        request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
        request
    }

    async fn stored(chat: &Chat, group: &str) -> usize {
        let rows = chat.ctx.db.query("SELECT id FROM chat_messages WHERE group_id = $1", &[&group]).await.unwrap();
        rows.len()
    }

    async fn insert(chat: &Chat, group: &str) {
        chat.ctx
            .db
            .execute(
                "INSERT INTO chat_messages (group_id, sender_geid, account_id, nickname, content, kind, created_at)
                 VALUES ($1, 1, 1, 'n', 'c', 1, 0)",
                &[&group],
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn deleting_a_groups_data_takes_only_its_messages() {
        let Some((db, ctx)) = TestDb::open(0, TABLES).await else { return };
        let chat = Chat { ctx };
        for group in ["a", "a", "b", "c"] {
            insert(&chat, group).await;
        }
        let request = Request::new(DeleteGroupDataRequest { group_ids: vec!["a".to_string(), "b".to_string()] });
        assert_eq!(chat.delete_group_data(request).await.unwrap().into_inner().deleted, 3);
        assert_eq!((stored(&chat, "a").await, stored(&chat, "b").await, stored(&chat, "c").await), (0, 0, 1));
        let again = Request::new(DeleteGroupDataRequest { group_ids: vec!["a".to_string()] });
        assert_eq!(chat.delete_group_data(again).await.unwrap().into_inner().deleted, 0, "asking again is harmless");
        db.drop_schema().await;
    }

    /// The real sc-social: chat asks it about the group, and once it destroys the group it has chat
    /// delete the messages.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn messages_follow_the_group_that_sc_social_keeps() {
        let Some((db, _)) = TestDb::open(2, TABLES).await else { return };
        let chat = Arc::new(Chat { ctx: db.context(TABLES).await });
        let chat_address = serve(Routes::new(GroupDataInternalServer::from_arc(chat.clone()))).await;
        let peers = sc_social::peers::PeerArgs { group_data: format!("chat={chat_address}"), ..nowhere() };
        let Some(social) = SocialProcess::spawn(&db, &peers).await else { return db.drop_schema().await };
        let chat = Chat { ctx: db.context_with(TABLES, &social.peers()).await };
        let (one, two) = (&db.accounts[0], &db.accounts[1]);
        let token = chat.ctx.subscription_key(one);
        let group = chat.ctx.peers.create_group(GroupType::Group, "g", "", vec![one.geid]).await.unwrap().group.id;

        let send = |content: &str| SendMessageRequest {
            group_id: group.clone(),
            content: content.to_string(),
            ..Default::default()
        };
        chat.send_message(signed(send("o7"), &token)).await.unwrap();
        let outsider = chat.send_message(signed(send("hi"), &chat.ctx.subscription_key(two))).await.unwrap_err();
        assert_eq!(outsider.code(), Code::PermissionDenied);
        let list = ListMessagesRequest { group_id: group.clone(), ..Default::default() };
        let listed = chat.list_messages(signed(list, &token)).await.unwrap().into_inner().messages;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].content, "o7");

        chat.ctx.peers.destroy_group(&group).await.unwrap();
        for _ in 0..100 {
            if stored(&chat, &group).await == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(stored(&chat, &group).await, 0, "sc-social had the messages deleted");
        let gone = chat.send_message(signed(send("late"), &token)).await.unwrap_err();
        assert_eq!(gone.code(), Code::NotFound);
        drop(social);
        db.drop_schema().await;
    }

    #[test]
    fn content_needs_text_within_the_limit() {
        assert!(check_content("o7").is_ok());
        assert!(check_content("   ").is_err());
        assert!(check_content(&"x".repeat(MAX_CONTENT_BYTES + 1)).is_err());
    }
}
