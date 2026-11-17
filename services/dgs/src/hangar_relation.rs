//! How a visitor is related to a hangar's owner, asked of the service that owns the relation, and the check that
//! combines it with the owner's `hangar_access::Access` (see docs/HANGARS.md).
//!
//! Parties belong to sc-social, so the DGS asks it over `SocialInternal.GetPartyMembers` and keeps nothing of its own.
//! Friends belong to sc-contacts: `ContactRelations` asks `ContactsInternal.ListFriends` too (`SocialRelations` alone never
//! answers `Relation::Friend`). All ids are character GEIDs, as sc-social keys its groups.

use sc_api::internal::{
    contacts_internal_client::ContactsInternalClient, social_internal_client::SocialInternalClient,
    GetPartyMembersRequest, ListFriendsRequest,
};
use tonic::{async_trait, transport::Channel};
use tracing::warn;

use crate::{
    hangar_access::{Access, Relation},
    player::{account_of_geid, player_geid},
};

/// Answers how `visitor` is related to `owner`.
#[async_trait]
pub trait RelationResolver: Send + Sync {
    async fn relation(&self, owner: u64, visitor: u64) -> Relation;
}

/// The resolver over sc-social's party. A social service that cannot answer makes everyone but the owner a stranger: a
/// guest is refused rather than let in on an assumption.
pub struct SocialRelations {
    social: SocialInternalClient<Channel>,
}

impl SocialRelations {
    pub fn new(social: SocialInternalClient<Channel>) -> Self {
        Self { social }
    }
}

#[async_trait]
impl RelationResolver for SocialRelations {
    async fn relation(&self, owner: u64, visitor: u64) -> Relation {
        if owner == visitor {
            return Relation::Owner;
        }
        match self
            .social
            .clone()
            .get_party_members(GetPartyMembersRequest { geid: owner })
            .await
        {
            Ok(reply)
                if reply
                    .get_ref()
                    .members
                    .iter()
                    .any(|member| member.geid == visitor) =>
            {
                Relation::Party
            }
            Ok(_) => Relation::Stranger,
            Err(status) => {
                warn!(code = ?status.code(), message = status.message(), owner, visitor, "party not had, visitor refused");
                Relation::Stranger
            }
        }
    }
}

/// The resolver over sc-social's party and sc-contacts' friends: a party member is `Party`, else a friend of the owner is
/// `Friend`. Friends are accounts there, the ids here are characters (`account_of_geid`). Contacts that cannot answer
/// leave a non-party visitor a stranger.
pub struct ContactRelations {
    parties: SocialRelations,
    contacts: ContactsInternalClient<Channel>,
}

impl ContactRelations {
    pub fn new(
        social: SocialInternalClient<Channel>,
        contacts: ContactsInternalClient<Channel>,
    ) -> Self {
        Self {
            parties: SocialRelations::new(social),
            contacts,
        }
    }
}

#[async_trait]
impl RelationResolver for ContactRelations {
    async fn relation(&self, owner: u64, visitor: u64) -> Relation {
        match self.parties.relation(owner, visitor).await {
            Relation::Stranger => {}
            known => return known,
        }
        let (Some(owner_account), Some(visitor_geid)) = (
            account_of_geid(owner).and_then(|account| u32::try_from(account).ok()),
            Some(visitor),
        ) else {
            return Relation::Stranger;
        };
        match self
            .contacts
            .clone()
            .list_friends(ListFriendsRequest {
                account_id: owner_account,
            })
            .await
        {
            Ok(reply)
                if reply
                    .get_ref()
                    .account_ids
                    .iter()
                    .any(|friend| player_geid(u64::from(*friend)) == Some(visitor_geid)) =>
            {
                Relation::Friend
            }
            Ok(_) => Relation::Stranger,
            Err(status) => {
                warn!(code = ?status.code(), message = status.message(), owner, visitor, "friends not had, visitor a stranger");
                Relation::Stranger
            }
        }
    }
}

/// Whether `visitor` may enter `owner`'s hangar under the owner's `access`.
pub async fn may_enter_hangar(
    relations: &dyn RelationResolver,
    access: &Access,
    owner: u64,
    visitor: u64,
) -> bool {
    access.may_enter(visitor, relations.relation(owner, visitor).await)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use sc_api::internal::{
        social_internal_server::{SocialInternal, SocialInternalServer},
        CreateGroupRequest, CreateGroupResponse, DestroyGroupRequest, DestroyGroupResponse,
        GetGroupsRequest, GetGroupsResponse, GetPartyMembersResponse, GetTeamsRequest,
        GroupMemberAccount, JoinGroupRequest, JoinGroupResponse, ListGroupIdsRequest,
        ListGroupIdsResponse, MoveToTeamRequest, MoveToTeamResponse, SyncTeamsRequest,
        SyncTeamsResponse,
    };
    use tonic::{
        transport::{server::TcpIncoming, Endpoint, Server},
        Request, Response, Status,
    };

    use super::*;

    const OWNER: u64 = 300_000_000_001;
    const PARTIED: u64 = 300_000_000_002;
    const FRIEND: u64 = 300_000_000_003;
    const STRANGER: u64 = 300_000_000_004;
    const UNUSED: &str = "not part of these tests";

    /// Relations by visitor, as a test states them.
    struct FakeRelations(BTreeMap<u64, Relation>);

    #[async_trait]
    impl RelationResolver for FakeRelations {
        async fn relation(&self, owner: u64, visitor: u64) -> Relation {
            if owner == visitor {
                Relation::Owner
            } else {
                self.0.get(&visitor).copied().unwrap_or(Relation::Stranger)
            }
        }
    }

    fn fake() -> FakeRelations {
        FakeRelations(BTreeMap::from([
            (PARTIED, Relation::Party),
            (FRIEND, Relation::Friend),
        ]))
    }

    #[tokio::test]
    async fn the_owner_enters_and_a_stranger_does_not() {
        let access = Access::default();
        assert!(may_enter_hangar(&fake(), &access, OWNER, OWNER).await);
        assert!(!may_enter_hangar(&fake(), &access, OWNER, STRANGER).await);
    }

    #[tokio::test]
    async fn party_and_friends_follow_the_owners_choices() {
        let access = Access {
            party: true,
            friends: false,
            ..Access::default()
        };
        assert!(may_enter_hangar(&fake(), &access, OWNER, PARTIED).await);
        assert!(!may_enter_hangar(&fake(), &access, OWNER, FRIEND).await);
    }

    #[tokio::test]
    async fn a_denied_party_member_is_refused() {
        let mut access = Access::default();
        access.denied.insert(PARTIED);
        assert!(!may_enter_hangar(&fake(), &access, OWNER, PARTIED).await);
    }

    /// A social service whose only party is the owner's and the partied visitor's.
    struct Parties;

    #[tonic::async_trait]
    impl SocialInternal for Parties {
        async fn get_groups(
            &self,
            _: Request<GetGroupsRequest>,
        ) -> Result<Response<GetGroupsResponse>, Status> {
            Err(Status::unimplemented(UNUSED))
        }
        async fn list_group_ids(
            &self,
            _: Request<ListGroupIdsRequest>,
        ) -> Result<Response<ListGroupIdsResponse>, Status> {
            Err(Status::unimplemented(UNUSED))
        }
        async fn get_teams(
            &self,
            _: Request<GetTeamsRequest>,
        ) -> Result<Response<GetGroupsResponse>, Status> {
            Err(Status::unimplemented(UNUSED))
        }
        async fn create_group(
            &self,
            _: Request<CreateGroupRequest>,
        ) -> Result<Response<CreateGroupResponse>, Status> {
            Err(Status::unimplemented(UNUSED))
        }
        async fn join_group(
            &self,
            _: Request<JoinGroupRequest>,
        ) -> Result<Response<JoinGroupResponse>, Status> {
            Err(Status::unimplemented(UNUSED))
        }
        async fn move_to_team(
            &self,
            _: Request<MoveToTeamRequest>,
        ) -> Result<Response<MoveToTeamResponse>, Status> {
            Err(Status::unimplemented(UNUSED))
        }
        async fn sync_teams(
            &self,
            _: Request<SyncTeamsRequest>,
        ) -> Result<Response<SyncTeamsResponse>, Status> {
            Err(Status::unimplemented(UNUSED))
        }
        async fn destroy_group(
            &self,
            _: Request<DestroyGroupRequest>,
        ) -> Result<Response<DestroyGroupResponse>, Status> {
            Err(Status::unimplemented(UNUSED))
        }
        async fn get_party_members(
            &self,
            request: Request<GetPartyMembersRequest>,
        ) -> Result<Response<GetPartyMembersResponse>, Status> {
            let in_party = [OWNER, PARTIED];
            let members = if in_party.contains(&request.into_inner().geid) {
                in_party
                    .iter()
                    .map(|&geid| GroupMemberAccount {
                        geid,
                        ..Default::default()
                    })
                    .collect()
            } else {
                Vec::new()
            };
            Ok(Response::new(GetPartyMembersResponse { members }))
        }
    }

    async fn social_client(serve: bool) -> SocialInternalClient<Channel> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        if serve {
            let server = Server::builder().add_service(SocialInternalServer::new(Parties));
            tokio::spawn(server.serve_with_incoming(TcpIncoming::from(listener)));
        } else {
            drop(listener);
        }
        SocialInternalClient::new(
            Endpoint::from_shared(format!("http://{address}"))
                .unwrap()
                .connect_lazy(),
        )
    }

    #[tokio::test]
    async fn social_relations_come_from_the_owners_party() {
        let relations = SocialRelations::new(social_client(true).await);
        assert_eq!(relations.relation(OWNER, OWNER).await, Relation::Owner);
        assert_eq!(relations.relation(OWNER, PARTIED).await, Relation::Party);
        assert_eq!(
            relations.relation(OWNER, FRIEND).await,
            Relation::Stranger,
            "SocialRelations alone does not ask for friends"
        );
        assert!(may_enter_hangar(&relations, &Access::default(), OWNER, PARTIED).await);
    }

    #[tokio::test]
    async fn a_social_service_that_is_not_there_refuses_guests_but_not_the_owner() {
        let relations = SocialRelations::new(social_client(false).await);
        assert!(may_enter_hangar(&relations, &Access::default(), OWNER, OWNER).await);
        assert!(!may_enter_hangar(&relations, &Access::default(), OWNER, PARTIED).await);
    }

    /// A contacts service whose only friendship is the owner's with `FRIEND`.
    struct Friends;

    #[tonic::async_trait]
    impl sc_api::internal::contacts_internal_server::ContactsInternal for Friends {
        async fn is_blocked(
            &self,
            _: Request<sc_api::internal::IsBlockedRequest>,
        ) -> Result<Response<sc_api::internal::IsBlockedResponse>, Status> {
            Err(Status::unimplemented(UNUSED))
        }
        async fn list_friends(
            &self,
            request: Request<ListFriendsRequest>,
        ) -> Result<Response<sc_api::internal::ListFriendsResponse>, Status> {
            let owner_account = account_of_geid(OWNER).unwrap() as u32;
            let account_ids = if request.into_inner().account_id == owner_account {
                vec![crate::player::account_of_geid(FRIEND).unwrap() as u32]
            } else {
                Vec::new()
            };
            Ok(Response::new(sc_api::internal::ListFriendsResponse {
                account_ids,
            }))
        }
    }

    async fn contacts_client() -> ContactsInternalClient<Channel> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = Server::builder().add_service(
            sc_api::internal::contacts_internal_server::ContactsInternalServer::new(Friends),
        );
        tokio::spawn(server.serve_with_incoming(TcpIncoming::from(listener)));
        ContactsInternalClient::new(
            Endpoint::from_shared(format!("http://{address}"))
                .unwrap()
                .connect_lazy(),
        )
    }

    #[tokio::test]
    async fn contact_relations_add_the_owners_friends() {
        let relations = ContactRelations::new(social_client(true).await, contacts_client().await);
        assert_eq!(relations.relation(OWNER, OWNER).await, Relation::Owner);
        assert_eq!(relations.relation(OWNER, PARTIED).await, Relation::Party);
        assert_eq!(relations.relation(OWNER, FRIEND).await, Relation::Friend);
        assert_eq!(
            relations.relation(OWNER, STRANGER).await,
            Relation::Stranger
        );
    }
}
