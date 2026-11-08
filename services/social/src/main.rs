//! sc.external.services.social.v1.SocialService: groups, invitations and game mode settings. sc-social
//! owns the groups (`social_*`): the chat, squad, lobby, call and presence services ask it through
//! scbackend.internal.SocialInternal, served on the same port, and it has them delete their rows of a
//! group it destroyed through `GroupDataInternal` (see `cleanup`).

mod cleanup;
mod store;

use std::{collections::HashMap, net::SocketAddr, process::ExitCode, sync::Arc};

use clap::Parser;
use prost::Message;
use sc_db::{internal, DatabaseArgs, Row};
use sc_api::internal::{ social_internal_server::{SocialInternal, SocialInternalServer}, CreateGroupRequest as InternalCreateGroupRequest, CreateGroupResponse as InternalCreateGroupResponse, DestroyGroupRequest as InternalDestroyGroupRequest, DestroyGroupResponse as InternalDestroyGroupResponse, GetGroupsRequest, GetGroupsResponse, GetTeamsRequest, InternalGroup, JoinGroupRequest, JoinGroupResponse, ListGroupIdsRequest, ListGroupIdsResponse, MoveToTeamRequest, MoveToTeamResponse, SyncTeamsRequest, SyncTeamsResponse, };
use sc_proto::{
    services::social::v1::{
    social_service_server::{SocialService, SocialServiceServer},
    AcceptInvitationRequest, AcceptInvitationResponse, AddGroupModeratorRequest, AddGroupModeratorResponse,
    CreateDmGroupRequest, CreateDmGroupResponse, CreateGroupRequest, CreateGroupResponse, DeclineInvitationRequest,
    DeclineInvitationResponse, DestroyGroupRequest, DestroyGroupResponse, GetClientGameModeSettingsRequest,
    GetClientGameModeSettingsResponse, GetGroupsByIdRequest, GetGroupsByIdResponse, GetInstanceInfoRequest,
    GetInstanceInfoResponse, GetSubscriptionKeyRequest, GetSubscriptionKeyResponse, Group, GroupMemberPreferences,
    GroupRenamed, GroupType, Invitation, InvitationDeclined, InvitationReceived, InvitationRevoked, InviteMode,
    InviteToPartyRequest, InviteToPartyResponse, KickGroupMemberRequest, KickGroupMemberResponse, LeaveGroupRequest,
    LeaveGroupResponse, ListGroupsRequest, ListGroupsResponse, ListPendingInvitationsRequest,
    ListPendingInvitationsResponse, MemberInvited, MemberReadyStatusChanged, ModeratorAdded, ModeratorRemoved,
    RemoveGroupModeratorRequest, RemoveGroupModeratorResponse, RenameGroupRequest, RenameGroupResponse,
    RevokeInvitationRequest, RevokeInvitationResponse, SendGroupInvitationRequest, SendGroupInvitationResponse,
    SetGroupPreferencesRequest, SetGroupPreferencesResponse, SetMemberReadyStatusRequest,
    SetMemberReadyStatusResponse, TransferGroupLeadershipRequest, TransferGroupLeadershipResponse,
    },
};
use sc_runtime::{state::Account, CommonArgs, MAX_MESSAGE_BYTES};
use sc_social::{
    groups::{self, member_of_account, GroupView},
    modes, now_ms,
    page::Page,
    peers::PeerArgs,
    presence,
    push::{Event, SOCIAL},
    transaction,
    tx::{Queries, Tx},
    urn, Context,
};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

const INVITATION_COLUMNS: &str = "id, group_id, from_geid, to_geid, kind, metadata, created_at";

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

struct Social {
    ctx: Context,
}

fn invitation(row: &Row) -> Result<Invitation, Status> {
    let metadata: Option<Vec<u8>> = row.get("metadata");
    let metadata = metadata.map_or(Ok(Invitation::default()), |bytes| Invitation::decode(bytes.as_slice()));
    Ok(Invitation {
        id: row.get("id"),
        from_urn: urn::player(row.get::<_, i64>("from_geid") as u64),
        to_urn: urn::player(row.get::<_, i64>("to_geid") as u64),
        group_id: row.get("group_id"),
        group_type: row.get("kind"),
        metadata: metadata.map_err(internal)?.metadata,
        created_at: row.get("created_at"),
    })
}

/// Whether this player may invite others, by the group's invite mode.
fn may_invite(group: &Group, geid: u64) -> bool {
    match group.settings.as_ref().map_or(InviteMode::Unspecified, |s| s.invite_mode()) {
        InviteMode::None => false,
        InviteMode::Anyone => groups::is_member(group, geid),
        InviteMode::LeaderOnly | InviteMode::Unspecified => groups::can_manage(group, geid),
    }
}

/// The group, for a change that only a member may make.
async fn locked_member(tx: &Tx<'_>, id: &str, geid: u64) -> Result<Group, Status> {
    let group = store::lock(tx, id).await?;
    if groups::is_member(&group, geid) {
        Ok(group)
    } else {
        Err(Status::permission_denied("not a member of this group"))
    }
}

/// The player of an invitation, when the service can still reach it.
fn invitee(ctx: &Context, player_urn: &str) -> Option<groups::Member> {
    ctx.directory.by_urn(player_urn).ok().map(|account| member_of_account(&account))
}

async fn invite(tx: &Tx<'_>, group: &Group, from: &Account, to: &Account) -> Result<Invitation, Status> {
    if groups::is_member(group, to.geid) {
        return Err(Status::already_exists(format!("{} is already in the group", to.handle)));
    }
    if tx.ctx.peers.is_blocked_by(to.account_id, from.account_id).await? {
        return Err(Status::permission_denied(format!("{} does not accept invitations", to.handle)));
    }
    let maximum = group.settings.as_ref().map_or(0, |s| s.max_member_count);
    if group.members.len() as i32 >= maximum {
        return Err(Status::failed_precondition("the group is full"));
    }
    let added = tx
        .execute(
            "INSERT INTO social_invitations (group_id, from_geid, to_geid, kind, created_at)
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
            &[&group.id, &(from.geid as i64), &(to.geid as i64), &group.r#type, &now_ms()],
        )
        .await?;
    let row = tx
        .query_opt(
            &format!("SELECT {INVITATION_COLUMNS} FROM social_invitations WHERE group_id = $1 AND to_geid = $2"),
            &[&group.id, &(to.geid as i64)],
        )
        .await?
        .ok_or_else(|| Status::aborted("invitation changed"))?;
    let invitation = invitation(&row)?;
    if added > 0 {
        let received = InvitationReceived { invitation: Some(invitation.clone()) };
        groups::to_player(tx, &member_of_account(to), &Event::new(SOCIAL, "InvitationReceived", &received));
        let invited = MemberInvited { invitation: Some(invitation.clone()) };
        let members = store::members(tx, &group.id).await?;
        groups::to_members(tx, &group.id, &members, &Event::new(SOCIAL, "MemberInvited", &invited));
    }
    Ok(invitation)
}

async fn find_invitation(db: &impl Queries, id: &str) -> Result<Invitation, Status> {
    let row = db
        .query_opt(&format!("SELECT {INVITATION_COLUMNS} FROM social_invitations WHERE id = $1"), &[&id])
        .await?
        .ok_or_else(|| Status::not_found(format!("no invitation {id}")))?;
    invitation(&row)
}

async fn delete_invitation(tx: &Tx<'_>, id: &str) -> Result<(), Status> {
    tx.execute("DELETE FROM social_invitations WHERE id = $1", &[&id]).await?;
    Ok(())
}

/// What this player set on these groups.
async fn preferences(db: &impl Queries, geid: u64, ids: &[String]) -> Result<Vec<GroupMemberPreferences>, Status> {
    let rows = db
        .query(
            "SELECT preferences FROM social_group_members
             WHERE geid = $1 AND group_id = ANY($2) AND preferences IS NOT NULL",
            &[&(geid as i64), &ids],
        )
        .await?;
    rows.iter()
        .map(|row| {
            let bytes: Vec<u8> = row.get("preferences");
            GroupMemberPreferences::decode(bytes.as_slice()).map_err(internal)
        })
        .collect()
}

impl Social {
    async fn set_moderator(
        &self,
        account: &Account,
        group_id: String,
        moderator_urn: String,
        moderator: bool,
    ) -> Result<(), Status> {
        let ctx = &self.ctx;
        let target = ctx.directory.by_urn(&moderator_urn)?;
        transaction!(ctx, tx, {
            let group = locked_member(&tx, &group_id, account.geid).await?;
            groups::require_leader(&group, account.geid)?;
            let changed = tx
                .execute(
                    "UPDATE social_group_members SET moderator = $3 WHERE group_id = $1 AND geid = $2",
                    &[&group_id, &(target.geid as i64), &moderator],
                )
                .await?;
            if changed == 0 {
                return Err(Status::not_found(format!("{} is not in the group", target.handle)));
            }
            let members = store::members(&tx, &group_id).await?;
            let event = if moderator {
                let added = ModeratorAdded { group_id: group_id.clone(), member_urn: moderator_urn.clone() };
                Event::new(SOCIAL, "ModeratorAdded", &added)
            } else {
                let removed = ModeratorRemoved { group_id: group_id.clone(), member_urn: moderator_urn.clone() };
                Event::new(SOCIAL, "ModeratorRemoved", &removed)
            };
            groups::to_members(&tx, &group_id, &members, &event);
            Ok(())
        })
    }
}

#[tonic::async_trait]
impl SocialService for Social {
    async fn create_dm_group(
        &self,
        request: Request<CreateDmGroupRequest>,
    ) -> Result<Response<CreateDmGroupResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let other = ctx.directory.by_urn(&request.into_inner().member_urn)?;
        if other.geid == account.geid {
            return Err(Status::invalid_argument("cannot message yourself"));
        }
        if ctx.peers.is_blocked_by(other.account_id, account.account_id).await? {
            return Err(Status::permission_denied(format!("{} does not accept messages", other.handle)));
        }
        let group = transaction!(ctx, tx, { store::direct_message(&tx, &account, &other).await })?;
        Ok(Response::new(CreateDmGroupResponse { group: Some(group) }))
    }

    async fn create_group(
        &self,
        request: Request<CreateGroupRequest>,
    ) -> Result<Response<CreateGroupResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let name = request.into_inner().name;
        let accounts = std::slice::from_ref(&account);
        let group = transaction!(ctx, tx, { store::create(&tx, GroupType::Group, &name, "", accounts).await })?;
        Ok(Response::new(CreateGroupResponse { group: Some(group) }))
    }

    async fn transfer_group_leadership(
        &self,
        request: Request<TransferGroupLeadershipRequest>,
    ) -> Result<Response<TransferGroupLeadershipResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        transaction!(ctx, tx, {
            let group = locked_member(&tx, &request.group_id, account.geid).await?;
            groups::require_leader(&group, account.geid)?;
            let next = ctx.directory.by_urn(&request.player_urn)?;
            if !groups::is_member(&group, next.geid) {
                return Err(Status::failed_precondition(format!("{} is not in the group", next.handle)));
            }
            let members = store::members(&tx, &group.id).await?;
            store::set_leader(&tx, &group.id, next.geid, &members).await
        })?;
        Ok(Response::new(TransferGroupLeadershipResponse {}))
    }

    async fn rename_group(
        &self,
        request: Request<RenameGroupRequest>,
    ) -> Result<Response<RenameGroupResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        transaction!(ctx, tx, {
            let group = locked_member(&tx, &request.group_id, account.geid).await?;
            if !groups::can_manage(&group, account.geid) {
                return Err(Status::permission_denied("only the leader or a moderator can rename a group"));
            }
            tx.execute("UPDATE social_groups SET name = $2 WHERE id = $1", &[&group.id, &request.name]).await?;
            let members = store::members(&tx, &group.id).await?;
            let renamed = GroupRenamed { group_id: group.id.clone(), group_name: request.name.clone() };
            groups::to_members(&tx, &group.id, &members, &Event::new(SOCIAL, "GroupRenamed", &renamed));
            Ok(())
        })?;
        Ok(Response::new(RenameGroupResponse {}))
    }

    async fn destroy_group(
        &self,
        request: Request<DestroyGroupRequest>,
    ) -> Result<Response<DestroyGroupResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let id = request.into_inner().group_id;
        transaction!(ctx, tx, {
            let group = locked_member(&tx, &id, account.geid).await?;
            groups::require_leader(&group, account.geid)?;
            store::destroy(&tx, &group).await
        })?;
        Ok(Response::new(DestroyGroupResponse {}))
    }

    /// A pending invitation lets a player look at the group before it decides.
    async fn get_groups_by_id(
        &self,
        request: Request<GetGroupsByIdRequest>,
    ) -> Result<Response<GetGroupsByIdResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let ids = request.into_inner().group_ids;
        let rows = ctx
            .db
            .query(
                "SELECT group_id FROM social_invitations WHERE to_geid = $1 AND group_id = ANY($2)",
                &[&(account.geid as i64), &ids],
            )
            .await
            .map_err(internal)?;
        let invited: Vec<String> = rows.iter().map(|row| row.get("group_id")).collect();
        let mut groups = store::load_many(ctx, &ids).await?;
        groups.retain(|group| groups::is_member(group, account.geid) || invited.contains(&group.id));
        let preferences = preferences(ctx, account.geid, &ids).await?;
        Ok(Response::new(GetGroupsByIdResponse { groups, preferences }))
    }

    async fn list_groups(
        &self,
        request: Request<ListGroupsRequest>,
    ) -> Result<Response<ListGroupsResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let page = Page::new(request.pagination.as_ref())?;
        let kinds = request.filters.map(|f| f.types).unwrap_or_default();
        let (ids, page_info) = page.slice(store::ids_of(ctx, account.geid, &kinds).await?);
        let groups = store::load_many(ctx, &ids).await?;
        let preferences = preferences(ctx, account.geid, &ids).await?;
        Ok(Response::new(ListGroupsResponse { groups, preferences, page_info: Some(page_info) }))
    }

    /// New values are merged over what the player set before.
    async fn set_group_preferences(
        &self,
        request: Request<SetGroupPreferencesRequest>,
    ) -> Result<Response<SetGroupPreferencesResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        transaction!(ctx, tx, {
            store::member_of(&tx, &request.group_id, account.geid).await?;
            let existing = preferences(&tx, account.geid, std::slice::from_ref(&request.group_id)).await?;
            let mut merged: HashMap<String, String> =
                existing.into_iter().next().map(|p| p.preferences).unwrap_or_default();
            merged.extend(request.preferences.clone());
            let stored = GroupMemberPreferences {
                group_id: request.group_id.clone(),
                member_urn: urn::player(account.geid),
                preferences: merged,
            };
            tx.execute(
                "UPDATE social_group_members SET preferences = $3 WHERE group_id = $1 AND geid = $2",
                &[&request.group_id, &(account.geid as i64), &stored.encode_to_vec()],
            )
            .await?;
            Ok(())
        })?;
        Ok(Response::new(SetGroupPreferencesResponse {}))
    }

    async fn leave_group(&self, request: Request<LeaveGroupRequest>) -> Result<Response<LeaveGroupResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let id = request.into_inner().group_id;
        transaction!(ctx, tx, {
            let group = locked_member(&tx, &id, account.geid).await?;
            if !group.settings.as_ref().is_some_and(|s| s.can_leave) {
                return Err(Status::failed_precondition("this group cannot be left"));
            }
            store::leave(&tx, &group, &member_of_account(&account), None).await
        })?;
        Ok(Response::new(LeaveGroupResponse {}))
    }

    async fn kick_group_member(
        &self,
        request: Request<KickGroupMemberRequest>,
    ) -> Result<Response<KickGroupMemberResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let kicked_by = urn::player(account.geid);
        transaction!(ctx, tx, {
            let group = locked_member(&tx, &request.group_id, account.geid).await?;
            let target = ctx.directory.by_urn(&request.member_urn)?;
            if !groups::can_manage(&group, account.geid) || groups::is_leader(&group, target.geid) {
                return Err(Status::permission_denied("not allowed to kick this member"));
            }
            store::leave(&tx, &group, &member_of_account(&target), Some(&kicked_by)).await
        })?;
        Ok(Response::new(KickGroupMemberResponse {}))
    }

    async fn send_group_invitation(
        &self,
        request: Request<SendGroupInvitationRequest>,
    ) -> Result<Response<SendGroupInvitationResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let invitations = transaction!(ctx, tx, {
            let group = store::lock(&tx, &request.group_id).await?;
            if !may_invite(&group, account.geid) {
                return Err(Status::permission_denied("not allowed to invite to this group"));
            }
            let mut invitations = Vec::with_capacity(request.member_urns.len());
            for member_urn in &request.member_urns {
                let target = ctx.directory.by_urn(member_urn)?;
                invitations.push(invite(&tx, &group, &account, &target).await?);
            }
            Ok(invitations)
        })?;
        Ok(Response::new(SendGroupInvitationResponse { invitations }))
    }

    /// The client calls this when it has no party, so the invitation creates one.
    async fn invite_to_party(
        &self,
        request: Request<InviteToPartyRequest>,
    ) -> Result<Response<InviteToPartyResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let target = ctx.directory.by_urn(&request.member_urn)?;
        if target.geid == account.geid {
            return Err(Status::invalid_argument("cannot invite yourself"));
        }
        let accounts = std::slice::from_ref(&account);
        // A party made for an invitation that fails goes with the rollback.
        let invitation = transaction!(ctx, tx, {
            let party = match store::one_of(&tx, account.geid, GroupType::Party).await? {
                Some(party) if may_invite(&party, account.geid) => store::lock(&tx, &party.id).await?,
                Some(_) => return Err(Status::permission_denied("only the party leader can invite")),
                None => store::create(&tx, GroupType::Party, &request.name, "", accounts).await?,
            };
            invite(&tx, &party, &account, &target).await
        })?;
        Ok(Response::new(InviteToPartyResponse { invitation: Some(invitation) }))
    }

    async fn accept_invitation(
        &self,
        request: Request<AcceptInvitationRequest>,
    ) -> Result<Response<AcceptInvitationResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let id = request.into_inner().invitation_id;
        transaction!(ctx, tx, {
            let invitation = find_invitation(&tx, &id).await?;
            if invitation.to_urn != urn::player(account.geid) {
                return Err(Status::permission_denied("not your invitation"));
            }
            store::join(&tx, &invitation.group_id, &account).await?;
            delete_invitation(&tx, &invitation.id).await
        })?;
        Ok(Response::new(AcceptInvitationResponse {}))
    }

    async fn decline_invitation(
        &self,
        request: Request<DeclineInvitationRequest>,
    ) -> Result<Response<DeclineInvitationResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let id = request.into_inner().invitation_id;
        transaction!(ctx, tx, {
            let invitation = find_invitation(&tx, &id).await?;
            if invitation.to_urn != urn::player(account.geid) {
                return Err(Status::permission_denied("not your invitation"));
            }
            store::lock(&tx, &invitation.group_id).await?;
            delete_invitation(&tx, &invitation.id).await?;
            if let Some(sender) = invitee(ctx, &invitation.from_urn) {
                let declined = InvitationDeclined {
                    invitation_id: invitation.id,
                    declined_by_urn: invitation.to_urn,
                    group_id: invitation.group_id.clone(),
                };
                groups::to_player(&tx, &sender, &Event::new(SOCIAL, "InvitationDeclined", &declined));
            }
            store::reap(&tx, &invitation.group_id).await
        })?;
        Ok(Response::new(DeclineInvitationResponse {}))
    }

    async fn revoke_invitation(
        &self,
        request: Request<RevokeInvitationRequest>,
    ) -> Result<Response<RevokeInvitationResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let id = request.into_inner().invitation_id;
        let caller_urn = urn::player(account.geid);
        transaction!(ctx, tx, {
            let invitation = find_invitation(&tx, &id).await?;
            let group = store::lock(&tx, &invitation.group_id).await?;
            if invitation.from_urn != caller_urn && !groups::can_manage(&group, account.geid) {
                return Err(Status::permission_denied("not allowed to revoke this invitation"));
            }
            delete_invitation(&tx, &invitation.id).await?;
            if let Some(target) = invitee(ctx, &invitation.to_urn) {
                let revoked = InvitationRevoked {
                    invitation_id: invitation.id,
                    revoked_by_urn: caller_urn.clone(),
                    group_id: group.id.clone(),
                };
                groups::to_player(&tx, &target, &Event::new(SOCIAL, "InvitationRevoked", &revoked));
            }
            store::reap(&tx, &group.id).await
        })?;
        Ok(Response::new(RevokeInvitationResponse {}))
    }

    async fn list_pending_invitations(
        &self,
        request: Request<ListPendingInvitationsRequest>,
    ) -> Result<Response<ListPendingInvitationsResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let page = Page::new(request.pagination.as_ref())?;
        let rows = ctx
            .db
            .query(
                &format!(
                    "SELECT {INVITATION_COLUMNS} FROM social_invitations
                     WHERE to_geid = $1 AND ($2 = 0 OR kind = $2) ORDER BY created_at, id LIMIT $3 OFFSET $4"
                ),
                &[&(account.geid as i64), &request.group_type, &page.fetch(), &page.offset],
            )
            .await
            .map_err(internal)?;
        let (rows, page_info) = page.finish(rows);
        let invitations = rows.iter().map(invitation).collect::<Result<_, _>>()?;
        Ok(Response::new(ListPendingInvitationsResponse { invitations, page_info: Some(page_info) }))
    }

    async fn get_instance_info(
        &self,
        request: Request<GetInstanceInfoRequest>,
    ) -> Result<Response<GetInstanceInfoResponse>, Status> {
        let ctx = &self.ctx;
        let caller = ctx.caller(request.metadata())?;
        let player = ctx.directory.by_urn(&request.into_inner().player_urn)?;
        let mut shard_info = presence::shard_info(ctx, &player).await?;
        // Any player may ask, and only the player's own client needs the server's address.
        if player.account_id != caller.account_id {
            shard_info.dgs_id.clear();
        }
        Ok(Response::new(GetInstanceInfoResponse { shard_info: Some(shard_info) }))
    }

    async fn get_subscription_key(
        &self,
        request: Request<GetSubscriptionKeyRequest>,
    ) -> Result<Response<GetSubscriptionKeyResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        Ok(Response::new(GetSubscriptionKeyResponse { subscription_key: self.ctx.subscription_key(&account) }))
    }

    async fn add_group_moderator(
        &self,
        request: Request<AddGroupModeratorRequest>,
    ) -> Result<Response<AddGroupModeratorResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let request = request.into_inner();
        self.set_moderator(&account, request.group_id, request.moderator_urn, true).await?;
        Ok(Response::new(AddGroupModeratorResponse {}))
    }

    async fn remove_group_moderator(
        &self,
        request: Request<RemoveGroupModeratorRequest>,
    ) -> Result<Response<RemoveGroupModeratorResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let request = request.into_inner();
        self.set_moderator(&account, request.group_id, request.moderator_urn, false).await?;
        Ok(Response::new(RemoveGroupModeratorResponse {}))
    }

    async fn set_member_ready_status(
        &self,
        request: Request<SetMemberReadyStatusRequest>,
    ) -> Result<Response<SetMemberReadyStatusResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let changed = ctx
            .db
            .execute(
                "UPDATE social_group_members SET ready = $3 WHERE group_id = $1 AND geid = $2",
                &[&request.group_id, &(account.geid as i64), &request.ready],
            )
            .await
            .map_err(internal)?;
        if changed == 0 {
            return Err(Status::permission_denied("not a member of this group"));
        }
        let members = store::members(ctx, &request.group_id).await?;
        let status = MemberReadyStatusChanged {
            group_id: request.group_id.clone(),
            member_urn: urn::player(account.geid),
            ready: request.ready,
        };
        let event = Event::new(SOCIAL, "MemberReadyStatusChanged", &status);
        groups::to_members(ctx, &request.group_id, &members, &event);
        Ok(Response::new(SetMemberReadyStatusResponse {}))
    }

    async fn get_client_game_mode_settings(
        &self,
        request: Request<GetClientGameModeSettingsRequest>,
    ) -> Result<Response<GetClientGameModeSettingsResponse>, Status> {
        self.ctx.caller(request.metadata())?;
        Ok(Response::new(GetClientGameModeSettingsResponse { game_mode_settings: modes::client_settings() }))
    }
}

/// The accounts of these players, in that order.
fn accounts(ctx: &Context, geids: &[u64]) -> Result<Vec<Account>, Status> {
    geids
        .iter()
        .map(|&geid| ctx.directory.by_geid(geid).ok_or_else(|| Status::not_found(format!("no player {geid}"))))
        .collect()
}

fn group_type(value: i32) -> Result<GroupType, Status> {
    GroupType::try_from(value).map_err(|_| Status::invalid_argument(format!("no group type {value}")))
}

fn internal_groups(views: Vec<GroupView>) -> GetGroupsResponse {
    GetGroupsResponse { groups: views.into_iter().map(InternalGroup::from).collect() }
}

/// What the other services ask of the groups. No account is checked: the caller is a backend
/// service, which checked its player's rights.
#[tonic::async_trait]
impl SocialInternal for Social {
    async fn get_groups(&self, request: Request<GetGroupsRequest>) -> Result<Response<GetGroupsResponse>, Status> {
        let views = store::views(&self.ctx, &request.into_inner().group_ids).await?;
        Ok(Response::new(internal_groups(views)))
    }

    async fn list_group_ids(
        &self,
        request: Request<ListGroupIdsRequest>,
    ) -> Result<Response<ListGroupIdsResponse>, Status> {
        let request = request.into_inner();
        let group_ids = store::ids_of(&self.ctx, request.geid, &request.types).await?;
        Ok(Response::new(ListGroupIdsResponse { group_ids }))
    }

    async fn get_teams(&self, request: Request<GetTeamsRequest>) -> Result<Response<GetGroupsResponse>, Status> {
        let teams = store::child_views(&self.ctx, &request.into_inner().group_id).await?;
        Ok(Response::new(internal_groups(teams)))
    }

    async fn create_group(
        &self,
        request: Request<InternalCreateGroupRequest>,
    ) -> Result<Response<InternalCreateGroupResponse>, Status> {
        let ctx = &self.ctx;
        let request = request.into_inner();
        let kind = group_type(request.r#type)?;
        let accounts = accounts(ctx, &request.member_geids)?;
        let group = transaction!(ctx, tx, {
            store::create(&tx, kind, &request.name, &request.parent_urn, &accounts).await
        })?;
        let view = store::view(ctx, &group.id).await?;
        Ok(Response::new(InternalCreateGroupResponse { group: Some(view.into()) }))
    }

    async fn join_group(&self, request: Request<JoinGroupRequest>) -> Result<Response<JoinGroupResponse>, Status> {
        let ctx = &self.ctx;
        let request = request.into_inner();
        let account = accounts(ctx, &[request.geid])?.remove(0);
        transaction!(ctx, tx, { store::join(&tx, &request.group_id, &account).await })?;
        Ok(Response::new(JoinGroupResponse {}))
    }

    async fn move_to_team(&self, request: Request<MoveToTeamRequest>) -> Result<Response<MoveToTeamResponse>, Status> {
        let ctx = &self.ctx;
        let request = request.into_inner();
        let (group, previous_team_id) =
            transaction!(ctx, tx, { store::move_to_team(&tx, &request.team_id, request.geid).await })?;
        Ok(Response::new(MoveToTeamResponse { group: Some(group.into()), previous_team_id }))
    }

    async fn sync_teams(&self, request: Request<SyncTeamsRequest>) -> Result<Response<SyncTeamsResponse>, Status> {
        let ctx = &self.ctx;
        let request = request.into_inner();
        let kind = group_type(request.team_type)?;
        transaction!(ctx, tx, {
            let group = store::lock(&tx, &request.group_id).await?;
            store::sync_teams(&tx, &group, request.count as usize, kind).await
        })?;
        Ok(Response::new(SyncTeamsResponse {}))
    }

    async fn destroy_group(
        &self,
        request: Request<InternalDestroyGroupRequest>,
    ) -> Result<Response<InternalDestroyGroupResponse>, Status> {
        let ctx = &self.ctx;
        let id = request.into_inner().group_id;
        transaction!(ctx, tx, {
            let group = store::lock(&tx, &id).await?;
            store::destroy(&tx, &group).await
        })?;
        Ok(Response::new(InternalDestroyGroupResponse {}))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let ctx = Context::open(&args.common, &args.database, &args.push, &args.peers, &store::schema()).await?;
    let service = Arc::new(Social { ctx });
    let worker = service.clone();
    tokio::spawn(async move { cleanup::run(&worker.ctx).await });
    let server = SocialServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    let internal = SocialInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, push = %args.push, "social listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use sc_proto::services::social::v1::{GroupMember, GroupSettings};
    use sc_social::{
        peers::PeerArgs,
        testing::{nowhere, FakeContacts, TestDb},
    };
    use tonic::Code;

    fn signed<T>(message: T, token: &str) -> Request<T> {
        let mut request = Request::new(message);
        request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
        request
    }

    /// Social on the test's schema, with sc-contacts standing in for the blocks.
    async fn with_contacts(db: &TestDb, contacts: &FakeContacts) -> Social {
        let peers = PeerArgs { contacts: contacts.serve().await, ..nowhere() };
        Social { ctx: db.context_with(store::TABLES, &peers).await }
    }

    async fn invite_all(social: &Social, token: &str, group_id: &str, accounts: &[Account]) -> Vec<Invitation> {
        let member_urns = accounts.iter().map(|account| urn::player(account.geid)).collect();
        let send = SendGroupInvitationRequest { group_id: group_id.to_string(), member_urns };
        social.send_group_invitation(signed(send, token)).await.unwrap().into_inner().invitations
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn racing_acceptances_stop_at_the_member_limit_and_keep_the_rest_invited() {
        let Some((db, _)) = TestDb::open(16, store::TABLES).await else { return };
        let social = Arc::new(with_contacts(&db, &FakeContacts::default()).await);
        let (leader, guests) = db.accounts.split_first().unwrap();
        let token = social.ctx.subscription_key(leader);
        let create = CreateGroupRequest { name: "race".to_string() };
        let id = SocialService::create_group(&*social, signed(create, &token)).await.unwrap().into_inner().group.unwrap().id;
        let invitations = invite_all(&social, &token, &id, guests).await;
        assert_eq!(invitations.len(), guests.len());
        let mut settings = groups::settings(GroupType::Group);
        settings.max_member_count = 4;
        social.ctx.execute("UPDATE social_groups SET settings = $2 WHERE id = $1", &[&id, &settings.encode_to_vec()])
            .await
            .unwrap();

        let mut tasks = tokio::task::JoinSet::new();
        for (guest, invitation) in guests.iter().zip(invitations) {
            let (social, token) = (social.clone(), social.ctx.subscription_key(guest));
            tasks.spawn(async move {
                let accept = AcceptInvitationRequest { invitation_id: invitation.id };
                social.accept_invitation(signed(accept, &token)).await.map(|_| ())
            });
        }
        let results = tasks.join_all().await;
        let joined = results.iter().filter(|result| result.is_ok()).count();
        assert_eq!(joined, 3, "{results:?}");
        assert_eq!(store::load(&social.ctx, &id).await.unwrap().members.len(), 4);
        let left = social.ctx.query("SELECT id FROM social_invitations WHERE group_id = $1", &[&id]).await.unwrap();
        assert_eq!(left.len(), guests.len() - joined, "a refused player keeps its invitation");
        db.drop_schema().await;
    }

    #[tokio::test]
    async fn a_party_made_for_a_failed_invitation_does_not_stay() {
        let Some((db, _)) = TestDb::open(2, store::TABLES).await else { return };
        let (from, to) = (&db.accounts[0], &db.accounts[1]);
        let contacts = FakeContacts::default();
        contacts.blocked.lock().unwrap().insert((to.account_id, from.account_id));
        let social = with_contacts(&db, &contacts).await;
        let token = social.ctx.subscription_key(from);
        let invite = InviteToPartyRequest { member_urn: urn::player(to.geid), ..Default::default() };
        let refused = social.invite_to_party(signed(invite, &token)).await.unwrap_err();
        assert_eq!(refused.code(), Code::PermissionDenied);
        assert!(store::one_of(&social.ctx, from.geid, GroupType::Party).await.unwrap().is_none());
        let groups = social.ctx.query("SELECT id FROM social_groups", &[]).await.unwrap();
        assert!(groups.is_empty());
        db.drop_schema().await;
    }

    #[tokio::test]
    async fn invitations_go_out_together_or_not_at_all() {
        let Some((db, _)) = TestDb::open(3, store::TABLES).await else { return };
        let social = with_contacts(&db, &FakeContacts::default()).await;
        let token = social.ctx.subscription_key(&db.accounts[0]);
        let create = CreateGroupRequest { name: "all or none".to_string() };
        let id = SocialService::create_group(&social, signed(create, &token)).await.unwrap().into_inner().group.unwrap().id;
        let member_urns = vec![urn::player(db.accounts[1].geid), urn::player(db.accounts[0].geid)];
        let send = SendGroupInvitationRequest { group_id: id.clone(), member_urns };
        let refused = social.send_group_invitation(signed(send, &token)).await.unwrap_err();
        assert_eq!(refused.code(), Code::AlreadyExists);
        let sent = social.ctx.query("SELECT id FROM social_invitations WHERE group_id = $1", &[&id]).await.unwrap();
        assert!(sent.is_empty());
        db.drop_schema().await;
    }

    fn group(mode: InviteMode) -> Group {
        Group {
            leader_urn: urn::player(1),
            members: vec![
                GroupMember { urn: urn::player(1), ..Default::default() },
                GroupMember { urn: urn::player(2), ..Default::default() },
            ],
            settings: Some(GroupSettings { invite_mode: mode as i32, ..Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn invite_mode_decides_who_may_invite() {
        assert!(may_invite(&group(InviteMode::LeaderOnly), 1));
        assert!(!may_invite(&group(InviteMode::LeaderOnly), 2));
        assert!(may_invite(&group(InviteMode::Anyone), 2));
        assert!(!may_invite(&group(InviteMode::Anyone), 3));
        assert!(!may_invite(&group(InviteMode::None), 1));
    }
}
