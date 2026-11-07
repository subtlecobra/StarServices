//! Groups and their members, which sc-social owns: no other service reads or writes `social_*`, and
//! they ask through `SocialInternal` instead. Joining and leaving push what the client needs to
//! follow: `JoinedGroup` and `LeftGroup` to the player, and member events to the group. A group with a
//! `parent_urn` is a team of its parent: it lives and dies with it, and whoever joins the parent gets a
//! place in one of its teams.
//!
//! Part of the sc-social binary, not of the library. Reads work on a [`Context`] or inside a
//! transaction. Changes take a [`Tx`]: run them in a [`transaction!`](sc_social::transaction),
//! together with whatever else belongs to the same change.

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use prost::Message;
use sc_db::{internal, Row};
use sc_proto::services::{
    presence::v1::Status as PresenceStatus,
    social::v1::{
        Group, GroupDestroyed, GroupMember, GroupSettings, GroupType, InvitationRevoked, JoinedGroup,
        LeadershipTransferred, LeftGroup, MemberJoined, MemberLeft,
    },
};
use sc_runtime::state::Account;
use sc_social::{
    groups::{is_leader, is_member, member_of_account, settings, to_members, to_player, GroupView, Member},
    now_ms,
    push::{Event, SOCIAL},
    tx::{Queries, Tx},
    urn,
};
use tonic::Status;
use tracing::warn;

/// sc-social's tables. A destroyed group leaves a row in `social_group_cleanups` for each service
/// with rows of that group, until that service has deleted them: see `cleanup`.
pub const TABLES: &str = "
CREATE TABLE IF NOT EXISTS social_groups (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
    name TEXT NOT NULL,
    kind INT NOT NULL,
    leader_geid BIGINT NOT NULL DEFAULT 0,
    settings BYTEA NOT NULL,
    parent_urn TEXT NOT NULL DEFAULT '',
    dm_key TEXT UNIQUE,
    created_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS social_groups_parent ON social_groups (parent_urn);
CREATE TABLE IF NOT EXISTS social_group_members (
    group_id TEXT NOT NULL,
    geid BIGINT NOT NULL,
    account_id BIGINT NOT NULL,
    name TEXT NOT NULL,
    moderator BOOLEAN NOT NULL DEFAULT false,
    ready BOOLEAN NOT NULL DEFAULT false,
    preferences BYTEA,
    joined_at BIGINT NOT NULL,
    PRIMARY KEY (group_id, geid)
);
CREATE INDEX IF NOT EXISTS social_group_members_geid ON social_group_members (geid);
CREATE TABLE IF NOT EXISTS social_invitations (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
    group_id TEXT NOT NULL,
    from_geid BIGINT NOT NULL,
    to_geid BIGINT NOT NULL,
    kind INT NOT NULL,
    metadata BYTEA,
    created_at BIGINT NOT NULL,
    UNIQUE (group_id, to_geid)
);
CREATE INDEX IF NOT EXISTS social_invitations_to ON social_invitations (to_geid);
CREATE TABLE IF NOT EXISTS social_group_cleanups (
    group_id TEXT NOT NULL,
    target TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (group_id, target)
);";

/// What sc-social applies at start.
pub fn schema() -> String {
    TABLES.to_string()
}

fn member_row(row: &Row) -> Member {
    Member {
        geid: row.get::<_, i64>("geid") as u64,
        account_id: row.get::<_, i64>("account_id") as u32,
        name: row.get("name"),
    }
}

fn group_row(row: &Row) -> Result<Group, Status> {
    let settings: Vec<u8> = row.get("settings");
    let leader = row.get::<_, i64>("leader_geid") as u64;
    Ok(Group {
        id: row.get("id"),
        name: row.get("name"),
        leader_urn: if leader == 0 { String::new() } else { urn::player(leader) },
        r#type: row.get("kind"),
        settings: Some(GroupSettings::decode(settings.as_slice()).map_err(internal)?),
        created_at: row.get("created_at"),
        parent_urn: row.get("parent_urn"),
        ..Default::default()
    })
}

/// The presence status of these accounts, from sc-presence. Without an answer everyone shows offline:
/// a group still loads while sc-presence is away.
async fn statuses(db: &impl Queries, accounts: &[u32]) -> HashMap<u32, i32> {
    match db.context().peers.presences(accounts).await {
        Ok(presences) => presences
            .into_iter()
            .map(|(id, p)| (id, p.presence.map_or(PresenceStatus::Offline as i32, |p| p.status)))
            .collect(),
        Err(error) => {
            warn!(%error, "presence of group members unknown");
            HashMap::new()
        }
    }
}

/// Fills in the members of `groups`, and returns them per group in the same order.
async fn add_members(db: &impl Queries, groups: &mut [Group]) -> Result<Vec<Vec<Member>>, Status> {
    let ids: Vec<&str> = groups.iter().map(|g| g.id.as_str()).collect();
    let rows = db
        .query(
            "SELECT group_id, geid, account_id, name, moderator, ready FROM social_group_members
             WHERE group_id = ANY($1) ORDER BY joined_at",
            &[&ids],
        )
        .await?;
    let mut accounts: Vec<u32> = rows.iter().map(|row| row.get::<_, i64>("account_id") as u32).collect();
    accounts.sort_unstable();
    accounts.dedup();
    let statuses = statuses(db, &accounts).await;
    let mut by_group: HashMap<String, Vec<&Row>> = HashMap::new();
    for row in &rows {
        by_group.entry(row.get("group_id")).or_default().push(row);
    }
    let mut all = Vec::with_capacity(groups.len());
    for group in groups {
        let mut members = Vec::new();
        for row in by_group.remove(&group.id).unwrap_or_default() {
            let member = member_row(row);
            let member_urn = urn::player(member.geid);
            if row.get("moderator") {
                group.moderators.push(member_urn.clone());
            }
            if row.get("ready") {
                group.ready_members.push(member_urn.clone());
            }
            group.members.push(GroupMember {
                urn: member_urn,
                display_name: member.name.clone(),
                presence_status: statuses.get(&member.account_id).copied().unwrap_or(PresenceStatus::Offline as i32),
            });
            members.push(member);
        }
        group.is_ready = !group.members.is_empty() && group.ready_members.len() == group.members.len();
        all.push(members);
    }
    Ok(all)
}

/// The groups with these ids and their members, in that order. Ids of no group are skipped.
pub async fn views(db: &impl Queries, ids: &[String]) -> Result<Vec<GroupView>, Status> {
    let rows = db
        .query(
            "SELECT id, name, kind, leader_geid, settings, parent_urn, created_at
             FROM social_groups WHERE id = ANY($1)",
            &[&ids],
        )
        .await?;
    let mut groups = rows.iter().map(group_row).collect::<Result<Vec<_>, _>>()?;
    let members = add_members(db, &mut groups).await?;
    let mut views: Vec<GroupView> =
        groups.into_iter().zip(members).map(|(group, members)| GroupView { group, members }).collect();
    views.sort_by_key(|view| ids.iter().position(|id| *id == view.group.id));
    Ok(views)
}

/// The groups with these ids, in that order. Ids of no group are skipped.
pub async fn load_many(db: &impl Queries, ids: &[String]) -> Result<Vec<Group>, Status> {
    Ok(views(db, ids).await?.into_iter().map(|view| view.group).collect())
}

pub async fn load(db: &impl Queries, id: &str) -> Result<Group, Status> {
    let found = load_many(db, &[id.to_string()]).await?.pop();
    found.ok_or_else(|| Status::not_found(format!("no group {id}")))
}

pub async fn view(db: &impl Queries, id: &str) -> Result<GroupView, Status> {
    let found = views(db, &[id.to_string()]).await?.pop();
    found.ok_or_else(|| Status::not_found(format!("no group {id}")))
}

/// The group, if `geid` is in it.
pub async fn member_of(db: &impl Queries, id: &str, geid: u64) -> Result<Group, Status> {
    let group = load(db, id).await?;
    if is_member(&group, geid) {
        Ok(group)
    } else {
        Err(Status::permission_denied("not a member of this group"))
    }
}

/// Takes the group for this change and reads it. Racing changes of one group queue here rather than
/// all failing at the end, which is what keeps retries of a busy group low.
pub async fn lock(tx: &Tx<'_>, id: &str) -> Result<Group, Status> {
    tx.query_opt("SELECT id FROM social_groups WHERE id = $1 FOR NO KEY UPDATE", &[&id]).await?;
    load(tx, id).await
}

pub async fn members(db: &impl Queries, id: &str) -> Result<Vec<Member>, Status> {
    let rows = db
        .query(
            "SELECT geid, account_id, name FROM social_group_members WHERE group_id = $1 ORDER BY joined_at",
            &[&id],
        )
        .await?;
    Ok(rows.iter().map(member_row).collect())
}

/// Ids of the groups `geid` is in, oldest membership first, limited to `kinds` unless that is empty.
pub async fn ids_of(db: &impl Queries, geid: u64, kinds: &[i32]) -> Result<Vec<String>, Status> {
    let rows = db
        .query(
            "SELECT g.id FROM social_group_members m JOIN social_groups g ON g.id = m.group_id
             WHERE m.geid = $1 AND (cardinality($2::int[]) = 0 OR g.kind = ANY($2)) ORDER BY m.joined_at",
            &[&(geid as i64), &kinds],
        )
        .await?;
    Ok(rows.iter().map(|row| row.get("id")).collect())
}

/// The one group of `kind` this player is in, such as its party.
pub async fn one_of(db: &impl Queries, geid: u64, kind: GroupType) -> Result<Option<Group>, Status> {
    match ids_of(db, geid, &[kind as i32]).await?.first() {
        Some(id) => load(db, id).await.map(Some),
        None => Ok(None),
    }
}

async fn child_ids(db: &impl Queries, id: &str) -> Result<Vec<String>, Status> {
    let rows = db
        .query("SELECT id FROM social_groups WHERE parent_urn = $1 ORDER BY created_at, id", &[&urn::group(id)])
        .await?;
    Ok(rows.iter().map(|row| row.get("id")).collect())
}

pub async fn children(db: &impl Queries, id: &str) -> Result<Vec<Group>, Status> {
    load_many(db, &child_ids(db, id).await?).await
}

/// The teams of a group with their members.
pub async fn child_views(db: &impl Queries, id: &str) -> Result<Vec<GroupView>, Status> {
    views(db, &child_ids(db, id).await?).await
}

async fn insert_member(tx: &Tx<'_>, id: &str, member: &Member) -> Result<u64, Status> {
    tx.execute(
        "INSERT INTO social_group_members (group_id, geid, account_id, name, joined_at)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        &[&id, &(member.geid as i64), &i64::from(member.account_id), &member.name, &now_ms()],
    )
    .await
}

/// A new group. The first of `accounts` leads it, unless its kind has no leader.
pub async fn create(
    tx: &Tx<'_>,
    kind: GroupType,
    name: &str,
    parent_urn: &str,
    accounts: &[Account],
) -> Result<Group, Status> {
    let settings = settings(kind);
    if settings.unique_for_type {
        for account in accounts {
            leave_kind(tx, account, kind).await?;
        }
    }
    let leader = accounts.first().filter(|_| settings.requires_leader).map_or(0, |a| a.geid as i64);
    let row = tx
        .query_opt(
            "INSERT INTO social_groups (name, kind, leader_geid, settings, parent_urn, created_at)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
            &[&name, &(kind as i32), &leader, &settings.encode_to_vec(), &parent_urn, &now_ms()],
        )
        .await?
        .ok_or_else(|| Status::internal("group not created"))?;
    let id: String = row.get("id");
    for account in accounts {
        insert_member(tx, &id, &member_of_account(account)).await?;
    }
    let group = load(tx, &id).await?;
    let joined = Event::new(SOCIAL, "JoinedGroup", &JoinedGroup { group: Some(group.clone()) });
    accounts.iter().for_each(|account| to_player(tx, &member_of_account(account), &joined));
    Ok(group)
}

/// The direct message group of two players, which exists once.
pub async fn direct_message(tx: &Tx<'_>, one: &Account, other: &Account) -> Result<Group, Status> {
    let (low, high) = (one.geid.min(other.geid), one.geid.max(other.geid));
    let key = format!("{low}:{high}");
    let existing = |row: Row| row.get::<_, String>("id");
    let lookup = "SELECT id FROM social_groups WHERE dm_key = $1";
    if let Some(row) = tx.query_opt(lookup, &[&key]).await? {
        return load(tx, &existing(row)).await;
    }
    let settings = settings(GroupType::Dm);
    let added = tx
        .query_opt(
            "INSERT INTO social_groups (name, kind, settings, dm_key, created_at)
             VALUES ('', $1, $2, $3, $4) ON CONFLICT DO NOTHING RETURNING id",
            &[&(GroupType::Dm as i32), &settings.encode_to_vec(), &key, &now_ms()],
        )
        .await?;
    let Some(row) = added else {
        let row = tx.query_opt(lookup, &[&key]).await?;
        let row = row.ok_or_else(|| Status::aborted("direct message group changed"))?;
        return load(tx, &existing(row)).await;
    };
    let id = existing(row);
    let members = [member_of_account(one), member_of_account(other)];
    for member in &members {
        insert_member(tx, &id, member).await?;
    }
    let group = load(tx, &id).await?;
    let joined = Event::new(SOCIAL, "JoinedGroup", &JoinedGroup { group: Some(group.clone()) });
    members.iter().for_each(|member| to_player(tx, member, &joined));
    Ok(group)
}

fn has_room(group: &Group) -> Result<(), Status> {
    let maximum = group.settings.as_ref().map_or(0, |s| s.max_member_count);
    if group.members.len() as i32 >= maximum {
        return Err(Status::failed_precondition("the group is full"));
    }
    Ok(())
}

/// Adds a player, moves it out of its other group of a kind that allows only one, and gives it the
/// emptiest team of the group if it has teams.
pub async fn join(tx: &Tx<'_>, id: &str, account: &Account) -> Result<(), Status> {
    let group = lock(tx, id).await?;
    if is_member(&group, account.geid) {
        return Err(Status::already_exists("already a member of this group"));
    }
    has_room(&group)?;
    if group.settings.as_ref().is_some_and(|s| s.unique_for_type) {
        leave_kind(tx, account, group.r#type()).await?;
    }
    let member = member_of_account(account);
    insert_member(tx, id, &member).await?;
    announce_join(tx, id, &member).await?;
    let teams = children(tx, id).await?;
    if let Some(team) = teams.iter().min_by_key(|team| team.members.len()) {
        insert_member(tx, &team.id, &member).await?;
        announce_join(tx, &team.id, &member).await?;
    }
    Ok(())
}

async fn announce_join(tx: &Tx<'_>, id: &str, member: &Member) -> Result<(), Status> {
    let group = load(tx, id).await?;
    let everyone = members(tx, id).await?;
    let member_urn = urn::player(member.geid);
    let entry = group.members.iter().find(|m| m.urn == member_urn).cloned();
    let joined = Event::new(SOCIAL, "MemberJoined", &MemberJoined { group_id: id.to_string(), member: entry });
    let others: Vec<Member> = everyone.into_iter().filter(|m| m.geid != member.geid).collect();
    to_members(tx, id, &others, &joined);
    to_player(tx, member, &Event::new(SOCIAL, "JoinedGroup", &JoinedGroup { group: Some(group) }));
    Ok(())
}

/// Moves a player to another team of the same parent.
pub async fn change_team(tx: &Tx<'_>, from: &Group, to: &Group, member: &Member) -> Result<(), Status> {
    has_room(to)?;
    remove_member(tx, from, member, None).await?;
    insert_member(tx, &to.id, member).await?;
    announce_join(tx, &to.id, member).await
}

/// Moves a member of the group that `team_id` is a team of into that team. Returns the group and the
/// team the player left, which is `team_id` itself when it was there already.
pub async fn move_to_team(tx: &Tx<'_>, team_id: &str, geid: u64) -> Result<(GroupView, String), Status> {
    let team = load(tx, team_id).await?;
    let parent = urn::group_id(&team.parent_urn).ok_or_else(|| Status::invalid_argument("not a team"))?;
    let parent = lock(tx, parent).await?;
    let member = members(tx, &parent.id).await?.into_iter().find(|member| member.geid == geid);
    let member = member.ok_or_else(|| Status::permission_denied("not in this group"))?;
    let teams = children(tx, &parent.id).await?;
    let target = teams.iter().find(|team| team.id == team_id);
    let target = target.ok_or_else(|| Status::not_found(format!("no team {team_id} in this lobby")))?;
    let current = teams.iter().find(|team| is_member(team, geid));
    let current = current.ok_or_else(|| Status::failed_precondition("the player has no team"))?;
    if current.id != target.id {
        change_team(tx, current, target, &member).await?;
    }
    Ok((view(tx, &parent.id).await?, current.id.clone()))
}

/// Gives a group `wanted` teams. Players of a team that goes move to the first one.
pub async fn sync_teams(tx: &Tx<'_>, group: &Group, wanted: usize, kind: GroupType) -> Result<(), Status> {
    if wanted == 0 {
        return Err(Status::invalid_argument("a group with teams has at least one"));
    }
    let teams = children(tx, &group.id).await?;
    for number in teams.len()..wanted {
        let name = format!("Team {}", number + 1);
        create(tx, kind, &name, &urn::group(&group.id), &[]).await?;
    }
    for team in teams.iter().skip(wanted) {
        for member in members(tx, &team.id).await? {
            let first = load(tx, &teams[0].id).await?;
            change_team(tx, team, &first, &member).await?;
        }
        destroy(tx, team).await?;
    }
    Ok(())
}

async fn leave_kind(tx: &Tx<'_>, account: &Account, kind: GroupType) -> Result<(), Status> {
    for id in ids_of(tx, account.geid, &[kind as i32]).await? {
        leave(tx, &lock(tx, &id).await?, &member_of_account(account), None).await?;
    }
    Ok(())
}

async fn remove_member(tx: &Tx<'_>, group: &Group, member: &Member, kicked_by: Option<&str>) -> Result<Vec<Member>, Status> {
    let removed = tx
        .execute(
            "DELETE FROM social_group_members WHERE group_id = $1 AND geid = $2",
            &[&group.id, &(member.geid as i64)],
        )
        .await?;
    if removed == 0 {
        return Err(Status::not_found("not a member of this group"));
    }
    let kicked_by_urn = kicked_by.unwrap_or_default().to_string();
    let left = LeftGroup { group_id: group.id.clone(), kicked: kicked_by.is_some(), kicked_by_urn: kicked_by_urn.clone() };
    to_player(tx, member, &Event::new(SOCIAL, "LeftGroup", &left));
    let remaining = members(tx, &group.id).await?;
    let gone = MemberLeft {
        group_id: group.id.clone(),
        member_urn: urn::player(member.geid),
        kicked: kicked_by.is_some(),
        kicked_by_urn,
    };
    to_members(tx, &group.id, &remaining, &Event::new(SOCIAL, "MemberLeft", &gone));
    Ok(remaining)
}

/// Takes a player out of a group and of its teams. The next oldest member takes over from a leader,
/// and a group left with too few players ends.
pub async fn leave(tx: &Tx<'_>, group: &Group, member: &Member, kicked_by: Option<&str>) -> Result<(), Status> {
    for team in children(tx, &group.id).await? {
        if is_member(&team, member.geid) {
            remove_member(tx, &team, member, kicked_by).await?;
        }
    }
    let remaining = remove_member(tx, group, member, kicked_by).await?;
    if is_leader(group, member.geid) {
        if let Some(next) = remaining.first() {
            set_leader(tx, &group.id, next.geid, &remaining).await?;
        }
    }
    reap(tx, &group.id).await
}

pub async fn set_leader(tx: &Tx<'_>, id: &str, geid: u64, members: &[Member]) -> Result<(), Status> {
    tx.execute("UPDATE social_groups SET leader_geid = $2 WHERE id = $1", &[&id, &(geid as i64)]).await?;
    let transferred = LeadershipTransferred { group_id: id.to_string(), player_urn: urn::player(geid) };
    to_members(tx, id, members, &Event::new(SOCIAL, "LeadershipTransferred", &transferred));
    Ok(())
}

/// Ends a group nobody needs any more: empty, or below its minimum with nobody invited to fill it.
pub async fn reap(tx: &Tx<'_>, id: &str) -> Result<(), Status> {
    let group = load(tx, id).await?;
    if !group.parent_urn.is_empty() {
        return Ok(());
    }
    let minimum = group.settings.as_ref().map_or(1, |s| s.min_member_count) as usize;
    if group.members.len() >= minimum {
        return Ok(());
    }
    let invited = tx.query_opt("SELECT id FROM social_invitations WHERE group_id = $1 LIMIT 1", &[&id]).await?;
    if group.members.is_empty() || invited.is_none() {
        destroy(tx, &group).await?;
    }
    Ok(())
}

/// Records, in the change that ends the group, that each service with rows per group still has to
/// delete its rows. The cleanup worker calls them once the change has committed.
async fn schedule_cleanup(tx: &Tx<'_>, id: &str) -> Result<(), Status> {
    for target in tx.ctx.peers.group_data_names() {
        tx.execute(
            "INSERT INTO social_group_cleanups (group_id, target, created_at) VALUES ($1, $2, $3)
             ON CONFLICT DO NOTHING",
            &[&id, &target, &now_ms()],
        )
        .await?;
    }
    tx.notify_after_commit();
    Ok(())
}

async fn remove_group(tx: &Tx<'_>, group: &Group, revoked_by: &str) -> Result<(), Status> {
    let everyone = members(tx, &group.id).await?;
    let invitations =
        tx.query("DELETE FROM social_invitations WHERE group_id = $1 RETURNING id, to_geid", &[&group.id]).await?;
    for row in &invitations {
        let revoked = InvitationRevoked {
            invitation_id: row.get("id"),
            revoked_by_urn: revoked_by.to_string(),
            group_id: group.id.clone(),
        };
        if let Some(invited) = tx.ctx.directory.by_geid(row.get::<_, i64>("to_geid") as u64) {
            to_player(tx, &member_of_account(&invited), &Event::new(SOCIAL, "InvitationRevoked", &revoked));
        }
    }
    schedule_cleanup(tx, &group.id).await?;
    tx.execute("DELETE FROM social_group_members WHERE group_id = $1", &[&group.id]).await?;
    tx.execute("DELETE FROM social_groups WHERE id = $1", &[&group.id]).await?;
    let destroyed = GroupDestroyed { group_id: group.id.clone() };
    to_members(tx, &group.id, &everyone, &Event::new(SOCIAL, "GroupDestroyed", &destroyed));
    let left = LeftGroup { group_id: group.id.clone(), ..Default::default() };
    let left = Event::new(SOCIAL, "LeftGroup", &left);
    everyone.iter().for_each(|member| to_player(tx, member, &left));
    Ok(())
}

/// Ends a group and its teams, with everything that belongs to them.
pub async fn destroy(tx: &Tx<'_>, group: &Group) -> Result<(), Status> {
    for team in children(tx, &group.id).await? {
        remove_group(tx, &team, &group.leader_urn).await?;
    }
    remove_group(tx, group, &group.leader_urn).await
}
