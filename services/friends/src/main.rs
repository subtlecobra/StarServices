//! sc.external.services.friends.v1.FriendService. It is the older face of ContactsService, over the
//! same relationships, so a friend added through either shows in both. sc-contacts owns those
//! relationships: each call goes to its ContactsService with the caller's own token, so sc-contacts
//! checks the caller as for any client, and this service keeps no data.

use std::{net::SocketAddr, process::ExitCode, time::Duration};

use clap::Parser;
use sc_proto::{
    common::api::v1::{PaginationArguments, Query},
    services::{
        contacts::v1::{
            self as contacts, contacts_service_client::ContactsServiceClient, AccountInfo, Contact, RelationshipType,
        },
        friends::v1::{
            block_account_request, friend_service_server::{FriendService, FriendServiceServer},
            send_friend_request_request, AcceptFriendRequestRequest, AcceptFriendRequestResponse, BlockAccountRequest,
            BlockAccountResponse, CancelFriendRequestRequest, CancelFriendRequestResponse,
            DeclineFriendRequestRequest, DeclineFriendRequestResponse, Friend, FriendInfo, FriendRequest,
            GetBlockedAccountRequest, GetBlockedAccountResponse, GetFriendListRequest, GetFriendListResponse,
            GetPendingFriendRequestRequest, GetPendingFriendRequestResponse, GetShardInfoRequest,
            GetShardInfoResponse, RemoveFriendRequest, RemoveFriendResponse, SearchAccountRequest,
            SearchAccountResponse, SendFriendRequestRequest, SendFriendRequestResponse, UnblockAccountRequest,
            UnblockAccountResponse,
        },
    },
};
use sc_runtime::{CommonArgs, MAX_MESSAGE_BYTES};
use sc_social::page::{matches, Page};
use tonic::{
    codec::CompressionEncoding,
    metadata::MetadataMap,
    transport::{Channel, Endpoint, Server},
    Request, Response, Status,
};
use tracing::info;

const AUTHORIZATION: &str = "authorization";
const TIMEOUT: Duration = Duration::from_secs(5);
/// The largest page sc-contacts gives, to read whole lists in few calls.
const FULL_PAGE: u32 = 500;

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
    /// Address of sc-contacts, which owns the relationships.
    #[arg(long, env = "SCBACKEND_CONTACTS", default_value = sc_social::peers::CONTACTS)]
    contacts: String,
}

struct Friends {
    contacts: ContactsServiceClient<Channel>,
}

fn friend_info(account: AccountInfo) -> FriendInfo {
    FriendInfo {
        nickname: account.nickname,
        display_name: account.display_name,
        avatar_url: account.avatar_url,
        account_id: account.account_id,
        player_id: account.player_id,
    }
}

fn field(info: &FriendInfo, name: &str) -> Option<String> {
    match name {
        "nickname" | "name" => Some(info.nickname.clone()),
        "display_name" => Some(info.display_name.clone()),
        "account_id" => Some(info.account_id.to_string()),
        "player_id" => Some(info.player_id.to_string()),
        _ => None,
    }
}

/// These requests carry a page both on their own and inside the query.
fn page(pagination: Option<&PaginationArguments>, query: Option<&Query>) -> Result<Page, Status> {
    Page::new(pagination.or(query.and_then(|q| q.pagination.as_ref())))
}

/// `message` as a call of the caller of `metadata`, which carries its token.
fn as_caller<T>(metadata: &MetadataMap, message: T) -> Result<Request<T>, Status> {
    let token = metadata.get(AUTHORIZATION).ok_or_else(|| Status::unauthenticated("missing or invalid token"))?;
    let mut request = Request::new(message);
    request.metadata_mut().insert(AUTHORIZATION, token.clone());
    Ok(request)
}

fn full_page(after: String) -> Option<PaginationArguments> {
    Some(PaginationArguments { first: FULL_PAGE, after })
}

/// Blocked accounts in the order they were blocked, as the table once gave them.
fn by_block_time(mut contacts: Vec<Contact>) -> Vec<Contact> {
    let blocked_at = |contact: &Contact| {
        let blocked = contact.relationships.iter().find(|r| r.r#type == RelationshipType::Blocked as i32);
        blocked.map_or(0, |r| r.created_at)
    };
    contacts.sort_by_key(blocked_at);
    contacts
}

impl Friends {
    fn client(&self) -> ContactsServiceClient<Channel> {
        self.contacts.clone()
    }

    /// Every contact of these kinds, through as many pages as there are.
    async fn all_contacts(&self, metadata: &MetadataMap, kind: RelationshipType) -> Result<Vec<Contact>, Status> {
        let mut all = Vec::new();
        let mut after = String::new();
        loop {
            let list = contacts::ListContactsRequest {
                relationship_types: vec![kind as i32],
                target_account_ids: Vec::new(),
                pagination: full_page(after),
            };
            let page = self.client().list_contacts(as_caller(metadata, list)?).await?.into_inner();
            all.extend(page.contacts);
            match page.page_info {
                Some(info) if info.has_next_page => after = info.end_cursor,
                _ => return Ok(all),
            }
        }
    }

    async fn all_pending(&self, metadata: &MetadataMap) -> Result<Vec<contacts::FriendRequest>, Status> {
        let mut all = Vec::new();
        let mut after = String::new();
        loop {
            let list = contacts::ListPendingFriendRequestsRequest { pagination: full_page(after) };
            let page = self.client().list_pending_friend_requests(as_caller(metadata, list)?).await?.into_inner();
            all.extend(page.pending_friend_requests);
            match page.page_info {
                Some(info) if info.has_next_page => after = info.end_cursor,
                _ => return Ok(all),
            }
        }
    }
}

#[tonic::async_trait]
impl FriendService for Friends {
    async fn get_friend_list(
        &self,
        request: Request<GetFriendListRequest>,
    ) -> Result<Response<GetFriendListResponse>, Status> {
        let contacts = self.all_contacts(request.metadata(), RelationshipType::Friend).await?;
        let request = request.into_inner();
        let page = page(request.pagination.as_ref(), request.query.as_ref())?;
        let filter = request.query.as_ref().and_then(|q| q.filter.as_ref());
        let friends = contacts
            .into_iter()
            .map(|contact| Friend { account: contact.account.map(friend_info), presence: contact.presence })
            .filter(|friend| {
                let info = friend.account.clone().unwrap_or_default();
                matches(filter, &|name| field(&info, name))
            })
            .collect();
        let (friends, page_info) = page.slice(friends);
        Ok(Response::new(GetFriendListResponse { friends, page_info: Some(page_info) }))
    }

    async fn search_account(
        &self,
        request: Request<SearchAccountRequest>,
    ) -> Result<Response<SearchAccountResponse>, Status> {
        let search = contacts::SearchAccountRequest {
            search: request.get_ref().name.clone(),
            pagination: request.get_ref().pagination.clone(),
        };
        let found = self.client().search_account(as_caller(request.metadata(), search)?).await?.into_inner();
        let accounts = found.accounts.into_iter().map(friend_info).collect();
        Ok(Response::new(SearchAccountResponse { accounts, page_info: found.page_info }))
    }

    async fn remove_friend(
        &self,
        request: Request<RemoveFriendRequest>,
    ) -> Result<Response<RemoveFriendResponse>, Status> {
        let remove = contacts::RemoveFriendRequest { account_id: request.get_ref().account_id };
        self.client().remove_friend(as_caller(request.metadata(), remove)?).await?;
        Ok(Response::new(RemoveFriendResponse {}))
    }

    async fn get_shard_info(
        &self,
        request: Request<GetShardInfoRequest>,
    ) -> Result<Response<GetShardInfoResponse>, Status> {
        let id = contacts::get_shard_info_request::Identifier::AccountId(request.get_ref().account_id as i32);
        let ask = contacts::GetShardInfoRequest { identifier: Some(id) };
        let found = self.client().get_shard_info(as_caller(request.metadata(), ask)?).await?.into_inner();
        Ok(Response::new(GetShardInfoResponse { shard_info: found.shard_info }))
    }

    async fn send_friend_request(
        &self,
        request: Request<SendFriendRequestRequest>,
    ) -> Result<Response<SendFriendRequestResponse>, Status> {
        use contacts::send_friend_request_request::Identifier;
        let identifier = match request.get_ref().identifier {
            Some(send_friend_request_request::Identifier::AccountId(id)) => Identifier::AccountId(id as i32),
            Some(send_friend_request_request::Identifier::PlayerId(id)) => Identifier::PlayerId(id),
            None => return Err(Status::invalid_argument("no account or player id")),
        };
        let send = contacts::SendFriendRequestRequest { identifier: Some(identifier) };
        let sent = self.client().send_friend_request(as_caller(request.metadata(), send)?).await?.into_inner();
        Ok(Response::new(SendFriendRequestResponse { request_id: sent.request.map(|r| r.id).unwrap_or_default() }))
    }

    async fn cancel_friend_request(
        &self,
        request: Request<CancelFriendRequestRequest>,
    ) -> Result<Response<CancelFriendRequestResponse>, Status> {
        let cancel = contacts::CancelFriendRequestRequest { request_id: request.get_ref().request_id.clone() };
        self.client().cancel_friend_request(as_caller(request.metadata(), cancel)?).await?;
        Ok(Response::new(CancelFriendRequestResponse {}))
    }

    async fn get_pending_friend_request(
        &self,
        request: Request<GetPendingFriendRequestRequest>,
    ) -> Result<Response<GetPendingFriendRequestResponse>, Status> {
        let pending = self.all_pending(request.metadata()).await?;
        let request = request.into_inner();
        let page = page(request.pagination.as_ref(), request.query.as_ref())?;
        let filter = request.query.as_ref().and_then(|q| q.filter.as_ref());
        let requests = pending
            .into_iter()
            .map(|request| FriendRequest {
                id: request.id,
                created_at: request.created_at,
                sender: request.sender.map(friend_info),
                receiver: request.receiver.map(friend_info),
            })
            .filter(|request| {
                let sender = request.sender.clone().unwrap_or_default();
                let receiver = request.receiver.clone().unwrap_or_default();
                matches(filter, &|name| match name {
                    "id" => Some(request.id.clone()),
                    "sender_id" => Some(sender.account_id.to_string()),
                    "receiver_id" => Some(receiver.account_id.to_string()),
                    _ => None,
                })
            })
            .collect();
        let (pending_friend_requests, page_info) = page.slice(requests);
        Ok(Response::new(GetPendingFriendRequestResponse { pending_friend_requests, page_info: Some(page_info) }))
    }

    async fn accept_friend_request(
        &self,
        request: Request<AcceptFriendRequestRequest>,
    ) -> Result<Response<AcceptFriendRequestResponse>, Status> {
        let accept = contacts::AcceptFriendRequestRequest { request_id: request.get_ref().request_id.clone() };
        self.client().accept_friend_request(as_caller(request.metadata(), accept)?).await?;
        Ok(Response::new(AcceptFriendRequestResponse {}))
    }

    async fn decline_friend_request(
        &self,
        request: Request<DeclineFriendRequestRequest>,
    ) -> Result<Response<DeclineFriendRequestResponse>, Status> {
        let decline = contacts::DeclineFriendRequestRequest { request_id: request.get_ref().request_id.clone() };
        self.client().decline_friend_request(as_caller(request.metadata(), decline)?).await?;
        Ok(Response::new(DeclineFriendRequestResponse {}))
    }

    async fn get_blocked_account(
        &self,
        request: Request<GetBlockedAccountRequest>,
    ) -> Result<Response<GetBlockedAccountResponse>, Status> {
        let blocked = by_block_time(self.all_contacts(request.metadata(), RelationshipType::Blocked).await?);
        let request = request.into_inner();
        let page = page(request.pagination.as_ref(), request.query.as_ref())?;
        let filter = request.query.as_ref().and_then(|q| q.filter.as_ref());
        let accounts = blocked
            .into_iter()
            .filter_map(|contact| contact.account.map(friend_info))
            .filter(|info| matches(filter, &|name| field(info, name)))
            .collect();
        let (blocked_accounts, page_info) = page.slice(accounts);
        Ok(Response::new(GetBlockedAccountResponse { blocked_accounts, page_info: Some(page_info) }))
    }

    async fn block_account(
        &self,
        request: Request<BlockAccountRequest>,
    ) -> Result<Response<BlockAccountResponse>, Status> {
        use contacts::set_unilateral_relationship_request::Identifier;
        let identifier = match request.get_ref().identifier {
            Some(block_account_request::Identifier::AccountId(id)) => Identifier::AccountId(id as i32),
            Some(block_account_request::Identifier::PlayerId(id)) => Identifier::PlayerId(id),
            None => return Err(Status::invalid_argument("no account or player id")),
        };
        let block = contacts::SetUnilateralRelationshipRequest {
            relationship_type: RelationshipType::Blocked as i32,
            identifier: Some(identifier),
        };
        self.client().set_unilateral_relationship(as_caller(request.metadata(), block)?).await?;
        Ok(Response::new(BlockAccountResponse {}))
    }

    async fn unblock_account(
        &self,
        request: Request<UnblockAccountRequest>,
    ) -> Result<Response<UnblockAccountResponse>, Status> {
        use contacts::remove_unilateral_relationship_request::Identifier;
        let unblock = contacts::RemoveUnilateralRelationshipRequest {
            relationship_type: RelationshipType::Blocked as i32,
            identifier: Some(Identifier::AccountId(request.get_ref().account_id as i32)),
        };
        self.client().remove_unilateral_relationship(as_caller(request.metadata(), unblock)?).await?;
        Ok(Response::new(UnblockAccountResponse {}))
    }
}

fn connect(address: &str) -> Result<ContactsServiceClient<Channel>, String> {
    let endpoint = Endpoint::from_shared(address.to_string()).map_err(|e| format!("--contacts {address}: {e}"))?;
    Ok(ContactsServiceClient::new(endpoint.timeout(TIMEOUT).connect_lazy()))
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Friends { contacts: connect(&args.contacts)? };
    let server = FriendServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, contacts = %args.contacts, "friends listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use contacts::{contacts_service_server::{ContactsService, ContactsServiceServer}, Relationship};
    use sc_proto::common::api::v1::PaginationInfo;
    use std::sync::{Arc, Mutex};
    use tonic::service::Routes;

    /// sc-contacts with 3 blocked accounts, blocked newest first by account id, given 2 per page. It
    /// records the token of each call.
    #[derive(Clone, Default)]
    struct FakeContacts {
        tokens: Arc<Mutex<Vec<String>>>,
    }

    fn blocked(id: u32, at: i64) -> Contact {
        Contact {
            account: Some(AccountInfo { account_id: id, nickname: format!("P{id}"), ..Default::default() }),
            relationships: vec![Relationship { r#type: RelationshipType::Blocked as i32, created_at: at }],
            ..Default::default()
        }
    }

    #[tonic::async_trait]
    impl ContactsService for FakeContacts {
        async fn list_contacts(
            &self,
            request: Request<contacts::ListContactsRequest>,
        ) -> Result<Response<contacts::ListContactsResponse>, Status> {
            let token = request.metadata().get(AUTHORIZATION).unwrap().to_str().unwrap().to_string();
            self.tokens.lock().unwrap().push(token);
            let all = vec![blocked(1, 30), blocked(2, 10), blocked(3, 20)];
            let after: usize = request.get_ref().pagination.as_ref().unwrap().after.parse().unwrap_or(0);
            let contacts: Vec<Contact> = all.into_iter().skip(after).take(2).collect();
            let end = after + contacts.len();
            let page_info = PaginationInfo { has_next_page: end < 3, end_cursor: end.to_string(), ..Default::default() };
            Ok(Response::new(contacts::ListContactsResponse { contacts, page_info: Some(page_info) }))
        }

        async fn search_account(&self, _: Request<contacts::SearchAccountRequest>) -> Result<Response<contacts::SearchAccountResponse>, Status> {
            Err(Status::unimplemented("search_account"))
        }

        async fn get_friend_subscription_key(&self, _: Request<contacts::GetFriendSubscriptionKeyRequest>) -> Result<Response<contacts::GetFriendSubscriptionKeyResponse>, Status> {
            Err(Status::unimplemented("get_friend_subscription_key"))
        }

        async fn remove_friend(&self, _: Request<contacts::RemoveFriendRequest>) -> Result<Response<contacts::RemoveFriendResponse>, Status> {
            Err(Status::unimplemented("remove_friend"))
        }

        async fn get_shard_info(&self, _: Request<contacts::GetShardInfoRequest>) -> Result<Response<contacts::GetShardInfoResponse>, Status> {
            Err(Status::unimplemented("get_shard_info"))
        }

        async fn send_friend_request(&self, _: Request<contacts::SendFriendRequestRequest>) -> Result<Response<contacts::SendFriendRequestResponse>, Status> {
            Err(Status::unimplemented("send_friend_request"))
        }

        async fn cancel_friend_request(&self, _: Request<contacts::CancelFriendRequestRequest>) -> Result<Response<contacts::CancelFriendRequestResponse>, Status> {
            Err(Status::unimplemented("cancel_friend_request"))
        }

        async fn list_pending_friend_requests(&self, _: Request<contacts::ListPendingFriendRequestsRequest>) -> Result<Response<contacts::ListPendingFriendRequestsResponse>, Status> {
            Err(Status::unimplemented("list_pending_friend_requests"))
        }

        async fn accept_friend_request(&self, _: Request<contacts::AcceptFriendRequestRequest>) -> Result<Response<contacts::AcceptFriendRequestResponse>, Status> {
            Err(Status::unimplemented("accept_friend_request"))
        }

        async fn decline_friend_request(&self, _: Request<contacts::DeclineFriendRequestRequest>) -> Result<Response<contacts::DeclineFriendRequestResponse>, Status> {
            Err(Status::unimplemented("decline_friend_request"))
        }

        async fn set_unilateral_relationship(&self, _: Request<contacts::SetUnilateralRelationshipRequest>) -> Result<Response<contacts::SetUnilateralRelationshipResponse>, Status> {
            Err(Status::unimplemented("set_unilateral_relationship"))
        }

        async fn remove_unilateral_relationship(&self, _: Request<contacts::RemoveUnilateralRelationshipRequest>) -> Result<Response<contacts::RemoveUnilateralRelationshipResponse>, Status> {
            Err(Status::unimplemented("remove_unilateral_relationship"))
        }

        async fn set_note(&self, _: Request<contacts::SetNoteRequest>) -> Result<Response<contacts::SetNoteResponse>, Status> {
            Err(Status::unimplemented("set_note"))
        }
    }

    #[tokio::test]
    async fn blocked_accounts_come_from_contacts_page_by_page_in_block_order() {
        let fake = FakeContacts::default();
        let address = sc_social::testing::serve(Routes::new(ContactsServiceServer::new(fake.clone()))).await;
        let friends = Friends { contacts: connect(&address).unwrap() };
        let mut request = Request::new(GetBlockedAccountRequest::default());
        request.metadata_mut().insert(AUTHORIZATION, "Bearer token".parse().unwrap());
        let blocked = friends.get_blocked_account(request).await.unwrap().into_inner().blocked_accounts;
        let ids: Vec<u32> = blocked.iter().map(|info| info.account_id).collect();
        assert_eq!(ids, [2, 3, 1]);
        assert_eq!(*fake.tokens.lock().unwrap(), ["Bearer token", "Bearer token"], "two pages, as the caller");

        let anonymous = friends.get_blocked_account(Request::new(GetBlockedAccountRequest::default())).await;
        assert_eq!(anonymous.unwrap_err().code(), tonic::Code::Unauthenticated);
        let refused = friends.remove_friend(Request::new(RemoveFriendRequest { account_id: 1 })).await;
        assert_eq!(refused.unwrap_err().code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn the_query_page_applies_when_the_request_has_none() {
        let query = Query {
            pagination: Some(PaginationArguments { first: 7, after: String::new() }),
            ..Default::default()
        };
        assert_eq!(page(None, Some(&query)).unwrap().limit, 7);
        let own = PaginationArguments { first: 3, after: String::new() };
        assert_eq!(page(Some(&own), Some(&query)).unwrap().limit, 3);
    }

    #[test]
    fn filters_read_the_friend_fields() {
        let info = FriendInfo { nickname: "Pilot".to_string(), account_id: 5, ..Default::default() };
        assert_eq!(field(&info, "name").as_deref(), Some("Pilot"));
        assert_eq!(field(&info, "account_id").as_deref(), Some("5"));
        assert_eq!(field(&info, "avatar_url"), None);
    }
}
