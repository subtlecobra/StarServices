//! sc.external.services.contacts.v1.ContactsService: what the client uses for friends, blocks and
//! notes. sc-contacts owns the relationships (`contact_*`): sc-friends calls ContactsService with its
//! caller's token, and sc-social and sc-presence ask scbackend.internal.ContactsInternal, served on
//! the same port.

mod relations;

use std::{net::SocketAddr, process::ExitCode, sync::Arc};

use clap::Parser;
use sc_db::DatabaseArgs;
use sc_proto::{
    internal::{
        contacts_internal_server::{ContactsInternal, ContactsInternalServer},
        IsBlockedRequest, IsBlockedResponse, ListFriendsRequest, ListFriendsResponse,
    },
    services::contacts::v1::{
    contacts_service_server::{ContactsService, ContactsServiceServer},
    get_shard_info_request, remove_unilateral_relationship_request, send_friend_request_request,
    set_note_request, set_unilateral_relationship_request, AcceptFriendRequestRequest, AcceptFriendRequestResponse,
    CancelFriendRequestRequest, CancelFriendRequestResponse, DeclineFriendRequestRequest,
    DeclineFriendRequestResponse, GetFriendSubscriptionKeyRequest, GetFriendSubscriptionKeyResponse,
    GetShardInfoRequest, GetShardInfoResponse, ListContactsRequest, ListContactsResponse,
    ListPendingFriendRequestsRequest, ListPendingFriendRequestsResponse, RelationshipType, RemoveFriendRequest,
    RemoveFriendResponse, RemoveUnilateralRelationshipRequest, RemoveUnilateralRelationshipResponse,
    SearchAccountRequest, SearchAccountResponse, SendFriendRequestRequest, SendFriendRequestResponse, SetNoteRequest,
    SetNoteResponse, SetUnilateralRelationshipRequest, SetUnilateralRelationshipResponse,
    },
};
use sc_runtime::{state::Account, CommonArgs, MAX_MESSAGE_BYTES};
use sc_social::{accounts::info, page::Page, peers::PeerArgs, Context};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

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

struct Contacts {
    ctx: Context,
}

/// The `identifier` oneof, which each request declares for itself.
enum Target {
    Account(i32),
    Player(u64),
    Missing,
}

macro_rules! target {
    ($identifier:expr, $module:ident) => {
        match $identifier {
            Some($module::Identifier::AccountId(id)) => Target::Account(id),
            Some($module::Identifier::PlayerId(id)) => Target::Player(id),
            None => Target::Missing,
        }
    };
}

impl Contacts {
    fn account(&self, target: Target) -> Result<Account, Status> {
        let directory = &self.ctx.directory;
        let found = match target {
            Target::Account(id) => directory.by_id(id as u32),
            Target::Player(id) => directory.by_geid(id),
            Target::Missing => return Err(Status::invalid_argument("no account or player id")),
        };
        found.ok_or_else(|| Status::not_found("no such account"))
    }
}

#[tonic::async_trait]
impl ContactsService for Contacts {
    async fn list_contacts(
        &self,
        request: Request<ListContactsRequest>,
    ) -> Result<Response<ListContactsResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let page = Page::new(request.pagination.as_ref())?;
        let targets: Vec<u32> = request.target_account_ids.iter().map(|&id| id as u32).collect();
        let all = relations::contacts(ctx, account.account_id, &request.relationship_types, &targets).await?;
        let (contacts, page_info) = page.slice(all);
        Ok(Response::new(ListContactsResponse { contacts, page_info: Some(page_info) }))
    }

    async fn search_account(
        &self,
        request: Request<SearchAccountRequest>,
    ) -> Result<Response<SearchAccountResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let page = Page::new(request.pagination.as_ref())?;
        let found = relations::search(&self.ctx, &account, &request.search).await?;
        let (accounts, page_info) = page.slice(found.iter().map(info).collect());
        Ok(Response::new(SearchAccountResponse { accounts, page_info: Some(page_info) }))
    }

    async fn get_friend_subscription_key(
        &self,
        request: Request<GetFriendSubscriptionKeyRequest>,
    ) -> Result<Response<GetFriendSubscriptionKeyResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        Ok(Response::new(GetFriendSubscriptionKeyResponse { subscription_key: self.ctx.subscription_key(&account) }))
    }

    async fn remove_friend(
        &self,
        request: Request<RemoveFriendRequest>,
    ) -> Result<Response<RemoveFriendResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let friend = self.account(Target::Account(request.into_inner().account_id as i32))?;
        relations::remove_friend(&self.ctx, &account, &friend).await?;
        Ok(Response::new(RemoveFriendResponse {}))
    }

    async fn get_shard_info(
        &self,
        request: Request<GetShardInfoRequest>,
    ) -> Result<Response<GetShardInfoResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let friend = self.account(target!(request.into_inner().identifier, get_shard_info_request))?;
        let shard_info = relations::friend_shard(&self.ctx, &account, &friend).await?;
        Ok(Response::new(GetShardInfoResponse { shard_info: Some(shard_info) }))
    }

    async fn send_friend_request(
        &self,
        request: Request<SendFriendRequestRequest>,
    ) -> Result<Response<SendFriendRequestResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let target = self.account(target!(request.into_inner().identifier, send_friend_request_request))?;
        let request = relations::send_request(&self.ctx, &account, &target).await?;
        Ok(Response::new(SendFriendRequestResponse { request: Some(request) }))
    }

    async fn cancel_friend_request(
        &self,
        request: Request<CancelFriendRequestRequest>,
    ) -> Result<Response<CancelFriendRequestResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        relations::cancel(&self.ctx, &account, &request.into_inner().request_id).await?;
        Ok(Response::new(CancelFriendRequestResponse {}))
    }

    async fn list_pending_friend_requests(
        &self,
        request: Request<ListPendingFriendRequestsRequest>,
    ) -> Result<Response<ListPendingFriendRequestsResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let page = Page::new(request.get_ref().pagination.as_ref())?;
        let (pending_friend_requests, page_info) = page.slice(relations::pending(&self.ctx, &account).await?);
        Ok(Response::new(ListPendingFriendRequestsResponse { pending_friend_requests, page_info: Some(page_info) }))
    }

    async fn accept_friend_request(
        &self,
        request: Request<AcceptFriendRequestRequest>,
    ) -> Result<Response<AcceptFriendRequestResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        relations::accept(&self.ctx, &account, &request.into_inner().request_id).await?;
        Ok(Response::new(AcceptFriendRequestResponse {}))
    }

    async fn decline_friend_request(
        &self,
        request: Request<DeclineFriendRequestRequest>,
    ) -> Result<Response<DeclineFriendRequestResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        relations::decline(&self.ctx, &account, &request.into_inner().request_id).await?;
        Ok(Response::new(DeclineFriendRequestResponse {}))
    }

    async fn set_unilateral_relationship(
        &self,
        request: Request<SetUnilateralRelationshipRequest>,
    ) -> Result<Response<SetUnilateralRelationshipResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let kind = request.relationship_type();
        let target = self.account(target!(request.identifier, set_unilateral_relationship_request))?;
        relations::set_unilateral(&self.ctx, &account, &target, kind).await?;
        Ok(Response::new(SetUnilateralRelationshipResponse { target_account_id: target.account_id as i32 }))
    }

    async fn remove_unilateral_relationship(
        &self,
        request: Request<RemoveUnilateralRelationshipRequest>,
    ) -> Result<Response<RemoveUnilateralRelationshipResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let kind: RelationshipType = request.relationship_type();
        let target = self.account(target!(request.identifier, remove_unilateral_relationship_request))?;
        relations::remove_unilateral(&self.ctx, &account, &target, kind).await?;
        Ok(Response::new(RemoveUnilateralRelationshipResponse { target_account_id: target.account_id as i32 }))
    }

    async fn set_note(&self, request: Request<SetNoteRequest>) -> Result<Response<SetNoteResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let target = self.account(target!(request.identifier, set_note_request))?;
        relations::set_note(&self.ctx, &account, &target, &request.content).await?;
        Ok(Response::new(SetNoteResponse {}))
    }
}

/// What the other services ask of the relationships. No account is checked: the caller is a backend
/// service.
#[tonic::async_trait]
impl ContactsInternal for Contacts {
    async fn is_blocked(&self, request: Request<IsBlockedRequest>) -> Result<Response<IsBlockedResponse>, Status> {
        let request = request.into_inner();
        let blocked =
            relations::has(&self.ctx, request.owner_account_id, request.target_account_id, relations::BLOCKED).await?;
        Ok(Response::new(IsBlockedResponse { blocked }))
    }

    async fn list_friends(&self, request: Request<ListFriendsRequest>) -> Result<Response<ListFriendsResponse>, Status> {
        let account_ids = relations::friend_ids(&self.ctx, request.into_inner().account_id).await?;
        Ok(Response::new(ListFriendsResponse { account_ids }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let ctx = Context::open(&args.common, &args.database, &args.push, &args.peers, relations::TABLES).await?;
    let service = Arc::new(Contacts { ctx });
    let server = ContactsServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    let internal = ContactsInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, push = %args.push, "contacts listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_proto::{
        internal::PlayerPresence,
        services::presence::v1::{Presence, Status as PresenceStatus},
    };
    use sc_social::testing::{nowhere, FakePresence, TestDb};

    fn signed<T>(message: T, token: &str) -> Request<T> {
        let mut request = Request::new(message);
        request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
        request
    }

    fn by_account(id: u32) -> Option<send_friend_request_request::Identifier> {
        Some(send_friend_request_request::Identifier::AccountId(id as i32))
    }

    /// Friends and blocks as the other services see them through ContactsInternal, and presence from
    /// sc-presence in the list.
    #[tokio::test]
    async fn other_services_see_friends_and_blocks() {
        let Some((db, _)) = TestDb::open(3, relations::TABLES).await else { return };
        let presence = FakePresence::default();
        let peers = PeerArgs { presence: presence.serve().await, ..nowhere() };
        let contacts = Contacts { ctx: db.context_with(relations::TABLES, &peers).await };
        let (one, two, three) = (&db.accounts[0], &db.accounts[1], &db.accounts[2]);
        let (token_one, token_two) = (contacts.ctx.subscription_key(one), contacts.ctx.subscription_key(two));

        let send = SendFriendRequestRequest { identifier: by_account(two.account_id) };
        let sent = contacts.send_friend_request(signed(send, &token_one)).await.unwrap().into_inner();
        let accept = AcceptFriendRequestRequest { request_id: sent.request.unwrap().id };
        contacts.accept_friend_request(signed(accept, &token_two)).await.unwrap();
        let block = SetUnilateralRelationshipRequest {
            relationship_type: RelationshipType::Blocked as i32,
            identifier: Some(set_unilateral_relationship_request::Identifier::AccountId(three.account_id as i32)),
        };
        contacts.set_unilateral_relationship(signed(block, &token_one)).await.unwrap();

        let friends = |account: u32| ListFriendsRequest { account_id: account };
        let listed = contacts.list_friends(Request::new(friends(one.account_id))).await.unwrap().into_inner();
        assert_eq!(listed.account_ids, [two.account_id]);
        let listed = contacts.list_friends(Request::new(friends(three.account_id))).await.unwrap().into_inner();
        assert!(listed.account_ids.is_empty());
        let blocked = |owner: u32, target: u32| IsBlockedRequest { owner_account_id: owner, target_account_id: target };
        let asked = contacts.is_blocked(Request::new(blocked(one.account_id, three.account_id))).await.unwrap();
        assert!(asked.into_inner().blocked);
        let asked = contacts.is_blocked(Request::new(blocked(three.account_id, one.account_id))).await.unwrap();
        assert!(!asked.into_inner().blocked, "a block goes one way");

        let online = Presence { status: PresenceStatus::Online as i32, activity: None };
        presence.set(PlayerPresence { account_id: two.account_id, presence: Some(online), environment: String::new() });
        let list = ListContactsRequest { relationship_types: vec![RelationshipType::Friend as i32], ..Default::default() };
        let listed = contacts.list_contacts(signed(list, &token_one)).await.unwrap().into_inner().contacts;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].presence.as_ref().unwrap().status, PresenceStatus::Online as i32);
        db.drop_schema().await;
    }

    /// Without sc-presence, friends still list, as offline.
    #[tokio::test]
    async fn contacts_list_while_presence_is_away() {
        let Some((db, ctx)) = TestDb::open(2, relations::TABLES).await else { return };
        let contacts = Contacts { ctx };
        let (one, two) = (&db.accounts[0], &db.accounts[1]);
        let (token_one, token_two) = (contacts.ctx.subscription_key(one), contacts.ctx.subscription_key(two));
        let send = SendFriendRequestRequest { identifier: by_account(two.account_id) };
        let sent = contacts.send_friend_request(signed(send, &token_one)).await.unwrap().into_inner();
        let accept = AcceptFriendRequestRequest { request_id: sent.request.unwrap().id };
        contacts.accept_friend_request(signed(accept, &token_two)).await.unwrap();
        let listed = contacts.list_contacts(signed(ListContactsRequest::default(), &token_one)).await.unwrap();
        let listed = listed.into_inner().contacts;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].presence.as_ref().unwrap().status, PresenceStatus::Offline as i32);
        db.drop_schema().await;
    }

    #[test]
    fn identifiers_become_targets() {
        let account = target!(Some(set_note_request::Identifier::AccountId(5)), set_note_request);
        assert!(matches!(account, Target::Account(5)));
        let player = target!(Some(set_note_request::Identifier::PlayerId(6)), set_note_request);
        assert!(matches!(player, Target::Player(6)));
        let none: Option<set_note_request::Identifier> = None;
        assert!(matches!(target!(none, set_note_request), Target::Missing));
    }
}
