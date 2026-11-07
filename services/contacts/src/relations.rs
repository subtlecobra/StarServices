//! Friendships, friend requests, one-way relationships and notes, which sc-contacts owns
//! (`contact_*`): ContactsService works on them here, sc-friends goes through ContactsService, and
//! sc-social and sc-presence ask through ContactsInternal. A friendship is two rows, one per direction,
//! written together. Pushes use the contacts messages, the ones the client listens for.

use std::collections::{btree_map::Entry, BTreeMap, HashMap};

use sc_db::{internal, Row};
use sc_proto::{
    common::shard::v1::ShardInfo,
    services::{
        contacts::v1::{
            Contact, FriendAdded, FriendRemoved, FriendRequest, FriendRequestCanceled, FriendRequestDeclined,
            FriendRequestReceived, Note, Relationship, RelationshipType,
        },
        presence::v1::Presence,
    },
};
use sc_runtime::state::Account;
use sc_social::{
    accounts::info,
    now_ms, presence,
    push::{contacts_topic, Event, CONTACTS},
    tx::Queries,
    Context,
};
use tonic::Status;
use tracing::warn;

pub const FRIEND: i32 = RelationshipType::Friend as i32;
pub const BLOCKED: i32 = RelationshipType::Blocked as i32;

/// sc-contacts' tables, which it alone reads and writes.
pub const TABLES: &str = "
CREATE TABLE IF NOT EXISTS contact_relationships (
    owner_id BIGINT NOT NULL,
    target_id BIGINT NOT NULL,
    kind INT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (owner_id, target_id, kind)
);
CREATE INDEX IF NOT EXISTS contact_relationships_target ON contact_relationships (target_id);
CREATE TABLE IF NOT EXISTS contact_friend_requests (
    id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
    sender_id BIGINT NOT NULL,
    receiver_id BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    UNIQUE (sender_id, receiver_id)
);
CREATE INDEX IF NOT EXISTS contact_friend_requests_receiver ON contact_friend_requests (receiver_id);
CREATE TABLE IF NOT EXISTS contact_notes (
    owner_id BIGINT NOT NULL,
    target_id BIGINT NOT NULL,
    content TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (owner_id, target_id)
);";

/// The presence of these accounts, from sc-presence. Without an answer they show offline, so that
/// contacts still list while sc-presence is away.
async fn presences(ctx: &Context, accounts: &[u32]) -> HashMap<u32, Presence> {
    match ctx.peers.presences(accounts).await {
        Ok(found) => found.into_iter().filter_map(|(id, p)| Some((id, p.presence?))).collect(),
        Err(error) => {
            warn!(%error, "presence of contacts unknown");
            HashMap::new()
        }
    }
}

fn notify(ctx: &Context, account: u32, name: &str, message: &impl prost::Message) {
    ctx.push.send(account, contacts_topic(account), &Event::new(CONTACTS, name, message));
}

fn request_row(ctx: &Context, row: &Row) -> Result<(FriendRequest, Account, Account), Status> {
    let account = |column: &str| {
        let id = row.get::<_, i64>(column) as u32;
        ctx.directory.by_id(id).ok_or_else(|| Status::not_found(format!("no account {id}")))
    };
    let (sender, receiver) = (account("sender_id")?, account("receiver_id")?);
    let request = FriendRequest {
        id: row.get("id"),
        created_at: row.get("created_at"),
        friends_in_common: 0,
        sender: Some(info(&sender)),
        receiver: Some(info(&receiver)),
    };
    Ok((request, sender, receiver))
}

pub async fn has(db: &impl Queries, owner: u32, target: u32, kind: i32) -> Result<bool, Status> {
    let row = db
        .query_opt(
            "SELECT 1 AS found FROM contact_relationships WHERE owner_id = $1 AND target_id = $2 AND kind = $3",
            &[&i64::from(owner), &i64::from(target), &kind],
        )
        .await?;
    Ok(row.is_some())
}

/// Whether `target` refuses `from`.
async fn is_blocked_by(db: &impl Queries, target: u32, from: u32) -> Result<bool, Status> {
    has(db, target, from, BLOCKED).await
}

async fn targets(ctx: &Context, owner: u32, kind: i32) -> Result<Vec<u32>, Status> {
    let rows = ctx
        .db
        .query(
            "SELECT target_id FROM contact_relationships WHERE owner_id = $1 AND kind = $2 ORDER BY created_at",
            &[&i64::from(owner), &kind],
        )
        .await
        .map_err(internal)?;
    Ok(rows.iter().map(|row| row.get::<_, i64>("target_id") as u32).collect())
}

pub async fn friend_ids(ctx: &Context, account: u32) -> Result<Vec<u32>, Status> {
    targets(ctx, account, FRIEND).await
}

/// Accounts matching `text` that `me` may see: not itself, nor anyone who blocked it.
pub async fn search(ctx: &Context, me: &Account, text: &str) -> Result<Vec<Account>, Status> {
    let rows = ctx
        .db
        .query(
            "SELECT owner_id FROM contact_relationships WHERE target_id = $1 AND kind = $2",
            &[&i64::from(me.account_id), &BLOCKED],
        )
        .await
        .map_err(internal)?;
    let blockers: Vec<u32> = rows.iter().map(|row| row.get::<_, i64>("owner_id") as u32).collect();
    let mut found = ctx.directory.search(text);
    found.retain(|a| a.account_id != me.account_id && !blockers.contains(&a.account_id));
    Ok(found)
}

/// Where a friend is. Only friends may ask.
pub async fn friend_shard(ctx: &Context, me: &Account, friend: &Account) -> Result<ShardInfo, Status> {
    if !has(ctx, me.account_id, friend.account_id, FRIEND).await? {
        return Err(Status::permission_denied("not friends"));
    }
    presence::shard_info(ctx, friend).await
}

async fn in_common(ctx: &Context, one: u32, other: u32) -> Result<u32, Status> {
    let row = ctx
        .db
        .query_opt(
            "SELECT count(*) AS shared FROM contact_relationships a JOIN contact_relationships b
                 ON a.target_id = b.target_id AND b.owner_id = $2 AND b.kind = $3
             WHERE a.owner_id = $1 AND a.kind = $3",
            &[&i64::from(one), &i64::from(other), &FRIEND],
        )
        .await
        .map_err(internal)?;
    Ok(row.map_or(0, |row| row.get::<_, i64>("shared") as u32))
}

async fn find_request(ctx: &Context, id: &str) -> Result<(FriendRequest, Account, Account), Status> {
    let row = ctx
        .db
        .query_opt("SELECT id, sender_id, receiver_id, created_at FROM contact_friend_requests WHERE id = $1", &[&id])
        .await
        .map_err(internal)?
        .ok_or_else(|| Status::not_found(format!("no friend request {id}")))?;
    request_row(ctx, &row)
}

async fn delete_request(ctx: &Context, id: &str) -> Result<(), Status> {
    ctx.db.execute("DELETE FROM contact_friend_requests WHERE id = $1", &[&id]).await.map_err(internal)?;
    Ok(())
}

pub async fn send_request(ctx: &Context, me: &Account, target: &Account) -> Result<FriendRequest, Status> {
    if me.account_id == target.account_id {
        return Err(Status::invalid_argument("cannot befriend yourself"));
    }
    if has(ctx, me.account_id, target.account_id, FRIEND).await? {
        return Err(Status::already_exists("already friends"));
    }
    if is_blocked_by(ctx, target.account_id, me.account_id).await?
        || is_blocked_by(ctx, me.account_id, target.account_id).await?
    {
        return Err(Status::permission_denied("one of the players blocked the other"));
    }
    let (mine, theirs) = (i64::from(me.account_id), i64::from(target.account_id));
    let select = "SELECT id, sender_id, receiver_id, created_at FROM contact_friend_requests
                  WHERE sender_id = $1 AND receiver_id = $2";
    // Two players asking each other means both want it.
    if let Some(row) = ctx.db.query_opt(select, &[&theirs, &mine]).await.map_err(internal)? {
        let (request, ..) = request_row(ctx, &row)?;
        accept(ctx, me, &request.id).await?;
        return Ok(request);
    }
    let added = ctx
        .db
        .execute(
            "INSERT INTO contact_friend_requests (sender_id, receiver_id, created_at)
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            &[&mine, &theirs, &now_ms()],
        )
        .await
        .map_err(internal)?;
    let row = ctx.db.query_opt(select, &[&mine, &theirs]).await.map_err(internal)?;
    let row = row.ok_or_else(|| Status::aborted("friend request changed"))?;
    let (mut request, ..) = request_row(ctx, &row)?;
    request.friends_in_common = in_common(ctx, me.account_id, target.account_id).await?;
    if added > 0 {
        let received = FriendRequestReceived { request_id: request.id.clone(), sender: Some(info(me)) };
        notify(ctx, target.account_id, "FriendRequestReceived", &received);
    }
    Ok(request)
}

pub async fn cancel(ctx: &Context, me: &Account, id: &str) -> Result<(), Status> {
    let (_, sender, receiver) = find_request(ctx, id).await?;
    if sender.account_id != me.account_id {
        return Err(Status::permission_denied("not your friend request"));
    }
    delete_request(ctx, id).await?;
    let canceled = FriendRequestCanceled { request_id: id.to_string(), sender: Some(info(me)) };
    notify(ctx, receiver.account_id, "FriendRequestCanceled", &canceled);
    Ok(())
}

pub async fn decline(ctx: &Context, me: &Account, id: &str) -> Result<(), Status> {
    let (_, sender, receiver) = find_request(ctx, id).await?;
    if receiver.account_id != me.account_id {
        return Err(Status::permission_denied("not your friend request"));
    }
    delete_request(ctx, id).await?;
    let declined = FriendRequestDeclined { request_id: id.to_string(), receiver: Some(info(me)) };
    notify(ctx, sender.account_id, "FriendRequestDeclined", &declined);
    Ok(())
}

async fn friend_contact(ctx: &Context, other: &Account, created_at: i64) -> Result<Contact, Status> {
    let presence = presences(ctx, &[other.account_id]).await.remove(&other.account_id);
    Ok(Contact {
        account: Some(info(other)),
        relationships: vec![Relationship { r#type: FRIEND, created_at }],
        note: None,
        presence: Some(presence.unwrap_or_else(presence::offline)),
    })
}

pub async fn accept(ctx: &Context, me: &Account, id: &str) -> Result<(), Status> {
    let (_, sender, receiver) = find_request(ctx, id).await?;
    if receiver.account_id != me.account_id {
        return Err(Status::permission_denied("not your friend request"));
    }
    let now = now_ms();
    let (one, other) = (i64::from(sender.account_id), i64::from(receiver.account_id));
    let mut client = ctx.db.client().await.map_err(internal)?;
    let transaction = client.transaction().await.map_err(internal)?;
    transaction
        .execute(
            "INSERT INTO contact_relationships (owner_id, target_id, kind, created_at)
             VALUES ($1, $2, $3, $4), ($2, $1, $3, $4) ON CONFLICT DO NOTHING",
            &[&one, &other, &FRIEND, &now],
        )
        .await
        .map_err(internal)?;
    transaction.execute("DELETE FROM contact_friend_requests WHERE id = $1", &[&id]).await.map_err(internal)?;
    transaction.commit().await.map_err(internal)?;
    drop(client);
    for (to, friend) in [(&sender, &receiver), (&receiver, &sender)] {
        let added = FriendAdded { friend: Some(friend_contact(ctx, friend, now).await?) };
        notify(ctx, to.account_id, "FriendAdded", &added);
    }
    Ok(())
}

async fn unfriend(ctx: &Context, me: &Account, target: &Account) -> Result<u64, Status> {
    let removed = ctx
        .db
        .execute(
            "DELETE FROM contact_relationships
             WHERE kind = $3 AND ((owner_id = $1 AND target_id = $2) OR (owner_id = $2 AND target_id = $1))",
            &[&i64::from(me.account_id), &i64::from(target.account_id), &FRIEND],
        )
        .await
        .map_err(internal)?;
    if removed > 0 {
        for (to, gone) in [(me, target), (target, me)] {
            let message = FriendRemoved { account_id: gone.account_id, player_id: gone.geid };
            notify(ctx, to.account_id, "FriendRemoved", &message);
        }
    }
    Ok(removed)
}

pub async fn remove_friend(ctx: &Context, me: &Account, target: &Account) -> Result<(), Status> {
    if unfriend(ctx, me, target).await? == 0 {
        return Err(Status::not_found("not friends"));
    }
    Ok(())
}

/// A one-way relationship. Blocking also ends any friendship and pending request between the two.
pub async fn set_unilateral(ctx: &Context, me: &Account, target: &Account, kind: RelationshipType) -> Result<(), Status> {
    if kind == RelationshipType::Friend || kind == RelationshipType::Unspecified {
        return Err(Status::invalid_argument("not a one-way relationship"));
    }
    if me.account_id == target.account_id {
        return Err(Status::invalid_argument("cannot relate to yourself"));
    }
    let (mine, theirs) = (i64::from(me.account_id), i64::from(target.account_id));
    ctx.db
        .execute(
            "INSERT INTO contact_relationships (owner_id, target_id, kind, created_at)
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
            &[&mine, &theirs, &(kind as i32), &now_ms()],
        )
        .await
        .map_err(internal)?;
    if kind == RelationshipType::Blocked {
        unfriend(ctx, me, target).await?;
        ctx.db
            .execute(
                "DELETE FROM contact_friend_requests
                 WHERE (sender_id = $1 AND receiver_id = $2) OR (sender_id = $2 AND receiver_id = $1)",
                &[&mine, &theirs],
            )
            .await
            .map_err(internal)?;
    }
    Ok(())
}

pub async fn remove_unilateral(ctx: &Context, me: &Account, target: &Account, kind: RelationshipType) -> Result<(), Status> {
    if kind == RelationshipType::Friend || kind == RelationshipType::Unspecified {
        return Err(Status::invalid_argument("not a one-way relationship"));
    }
    ctx.db
        .execute(
            "DELETE FROM contact_relationships WHERE owner_id = $1 AND target_id = $2 AND kind = $3",
            &[&i64::from(me.account_id), &i64::from(target.account_id), &(kind as i32)],
        )
        .await
        .map_err(internal)?;
    Ok(())
}

/// Requests from and to this player, oldest first.
pub async fn pending(ctx: &Context, me: &Account) -> Result<Vec<FriendRequest>, Status> {
    let rows = ctx
        .db
        .query(
            "SELECT id, sender_id, receiver_id, created_at FROM contact_friend_requests
             WHERE sender_id = $1 OR receiver_id = $1 ORDER BY created_at, id",
            &[&i64::from(me.account_id)],
        )
        .await
        .map_err(internal)?;
    let mut requests = Vec::with_capacity(rows.len());
    for row in &rows {
        let (mut request, sender, receiver) = request_row(ctx, row)?;
        request.friends_in_common = in_common(ctx, sender.account_id, receiver.account_id).await?;
        requests.push(request);
    }
    Ok(requests)
}

/// An empty note clears it.
pub async fn set_note(ctx: &Context, me: &Account, target: &Account, content: &str) -> Result<(), Status> {
    let (mine, theirs) = (i64::from(me.account_id), i64::from(target.account_id));
    if content.is_empty() {
        ctx.db
            .execute("DELETE FROM contact_notes WHERE owner_id = $1 AND target_id = $2", &[&mine, &theirs])
            .await
            .map_err(internal)?;
        return Ok(());
    }
    ctx.db
        .execute(
            "INSERT INTO contact_notes (owner_id, target_id, content, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $4)
             ON CONFLICT (owner_id, target_id) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at",
            &[&mine, &theirs, &content, &now_ms()],
        )
        .await
        .map_err(internal)?;
    Ok(())
}

/// The contact of `target`, new if needed. A deleted account is no contact.
fn contact_entry<'a>(ctx: &Context, contacts: &'a mut BTreeMap<u32, Contact>, target: u32) -> Option<&'a mut Contact> {
    match contacts.entry(target) {
        Entry::Occupied(entry) => Some(entry.into_mut()),
        Entry::Vacant(entry) => {
            let account = ctx.directory.by_id(target)?;
            Some(entry.insert(Contact { account: Some(info(&account)), ..Default::default() }))
        }
    }
}

fn keep(contact: &Contact, kinds: &[i32], targets: &[u32]) -> bool {
    let kind_passes = kinds.is_empty() || contact.relationships.iter().any(|r| kinds.contains(&r.r#type));
    let id = contact.account.as_ref().map_or(0, |a| a.account_id);
    kind_passes && (targets.is_empty() || targets.contains(&id))
}

/// Everyone this player has a relationship or a note on, by account id. Only friends show presence,
/// so nothing else reveals who is online. Empty `kinds` or `targets` mean no restriction.
pub async fn contacts(ctx: &Context, me: u32, kinds: &[i32], targets: &[u32]) -> Result<Vec<Contact>, Status> {
    let owner = i64::from(me);
    let relationships = ctx
        .db
        .query(
            "SELECT target_id, kind, created_at FROM contact_relationships WHERE owner_id = $1 ORDER BY created_at",
            &[&owner],
        )
        .await
        .map_err(internal)?;
    let notes = ctx
        .db
        .query("SELECT target_id, content, created_at, updated_at FROM contact_notes WHERE owner_id = $1", &[&owner])
        .await
        .map_err(internal)?;
    let mut contacts: BTreeMap<u32, Contact> = BTreeMap::new();
    for row in &relationships {
        let target = row.get::<_, i64>("target_id") as u32;
        let Some(contact) = contact_entry(ctx, &mut contacts, target) else {
            continue;
        };
        contact.relationships.push(Relationship { r#type: row.get("kind"), created_at: row.get("created_at") });
    }
    for row in &notes {
        let target = row.get::<_, i64>("target_id") as u32;
        let Some(contact) = contact_entry(ctx, &mut contacts, target) else {
            continue;
        };
        contact.note = Some(Note {
            content: row.get("content"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        });
    }
    let mut list: Vec<Contact> = contacts.into_values().filter(|c| keep(c, kinds, targets)).collect();
    let friends: Vec<u32> = list
        .iter()
        .filter(|c| c.relationships.iter().any(|r| r.r#type == FRIEND))
        .filter_map(|c| c.account.as_ref().map(|a| a.account_id))
        .collect();
    let mut presences = presences(ctx, &friends).await;
    for contact in &mut list {
        let id = contact.account.as_ref().map_or(0, |a| a.account_id);
        contact.presence = Some(presences.remove(&id).unwrap_or_else(presence::offline));
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_proto::services::contacts::v1::AccountInfo;

    fn contact(id: u32, kind: i32) -> Contact {
        Contact {
            account: Some(AccountInfo { account_id: id, ..Default::default() }),
            relationships: vec![Relationship { r#type: kind, created_at: 0 }],
            ..Default::default()
        }
    }

    #[test]
    fn contacts_filter_by_kind_and_target() {
        let friend = contact(1, FRIEND);
        assert!(keep(&friend, &[], &[]));
        assert!(keep(&friend, &[FRIEND], &[1]));
        assert!(!keep(&friend, &[BLOCKED], &[]));
        assert!(!keep(&friend, &[], &[2]));
    }
}
