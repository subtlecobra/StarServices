//! Group changes against a real database, the internal RPCs and the cleanup of a destroyed group's rows in
//! the services that own them. They need `SCBACKEND_TEST_POSTGRES`, see `sc_social::testing`.

use std::{sync::Arc, time::Duration};

use sc_proto::internal::{
    social_internal_server::{SocialInternal, SocialInternalServer},
    CreateGroupRequest, DestroyGroupRequest, GetGroupsRequest, GetTeamsRequest, JoinGroupRequest, ListGroupIdsRequest,
    MoveToTeamRequest, SyncTeamsRequest,
};
use sc_social::{
    peers::{PeerArgs, Peers},
    testing::{nowhere, serve, FakeGroupData, TestDb},
    transaction,
    tx::is_conflict,
    Context,
};
use tokio::{sync::Barrier, task::JoinSet};
use tonic::{service::Routes, Code, Request};

use super::*;
use crate::{cleanup, Social};

const REPLICAS: usize = 3;
const TARGETS: [&str; 4] = ["chat", "squad", "custom_lobby", "rtc"];

async fn new_group(ctx: &Context, kind: GroupType, leader: &Account) -> String {
    let leader = std::slice::from_ref(leader);
    transaction!(ctx, tx, { create(&tx, kind, "test", "", leader).await }).unwrap().id
}

async fn set_limit(ctx: &Context, id: &str, limit: i32) {
    let mut settings = settings(GroupType::Group);
    settings.max_member_count = limit;
    ctx.db.execute("UPDATE social_groups SET settings = $2 WHERE id = $1", &[&id, &settings.encode_to_vec()]).await.unwrap();
}

async fn count(ctx: &Context, sql: &str, id: &str) -> i64 {
    ctx.query_opt(sql, &[&id]).await.unwrap().map_or(0, |row: Row| row.get::<_, i64>(0))
}

async fn member_count(ctx: &Context, id: &str) -> i64 {
    count(ctx, "SELECT count(*) FROM social_group_members WHERE group_id = $1", id).await
}

/// The services this group's rows still wait on.
async fn pending_cleanups(ctx: &Context, id: &str) -> Vec<String> {
    let rows = ctx
        .query("SELECT target FROM social_group_cleanups WHERE group_id = $1 ORDER BY target", &[&id])
        .await
        .unwrap();
    rows.iter().map(|row| row.get("target")).collect()
}

async fn contexts(db: &TestDb) -> Vec<Arc<Context>> {
    let mut contexts = Vec::with_capacity(REPLICAS);
    for _ in 0..REPLICAS {
        contexts.push(Arc::new(db.context(TABLES).await));
    }
    contexts
}

/// Runs one change per account at the same moment, across several pools, and returns the outcomes.
async fn race<F, Fut>(contexts: &[Arc<Context>], accounts: &[Account], change: F) -> Vec<Result<(), Status>>
where
    F: Fn(Arc<Context>, Account) -> Fut,
    Fut: std::future::Future<Output = Result<(), Status>> + Send + 'static,
{
    let start = Arc::new(Barrier::new(accounts.len()));
    let mut tasks = JoinSet::new();
    for (n, account) in accounts.iter().enumerate() {
        let change = change(contexts[n % contexts.len()].clone(), account.clone());
        let start = start.clone();
        tasks.spawn(async move {
            start.wait().await;
            change.await
        });
    }
    tasks.join_all().await
}

/// One stand-in per service with rows per group, and the peers that reach them.
async fn group_data_services() -> (Vec<FakeGroupData>, PeerArgs) {
    let mut fakes = Vec::new();
    let mut list = Vec::new();
    for name in TARGETS {
        let fake = FakeGroupData::default();
        list.push(format!("{name}={}", fake.serve().await));
        fakes.push(fake);
    }
    (fakes, PeerArgs { group_data: list.join(","), ..nowhere() })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn racing_joins_stop_at_the_member_limit() {
    let Some((db, ctx)) = TestDb::open(41, TABLES).await else { return };
    let contexts = contexts(&db).await;
    let (leader, joiners) = db.accounts.split_first().unwrap();
    for limit in [2, 5, 12] {
        let id = new_group(&ctx, GroupType::Group, leader).await;
        set_limit(&ctx, &id, limit).await;
        let results = race(&contexts, joiners, |ctx, account| {
            let id = id.clone();
            async move { transaction!(&ctx, tx, { join(&tx, &id, &account).await }) }
        })
        .await;
        let joined = results.iter().filter(|result| result.is_ok()).count();
        let full = results.iter().filter(|r| r.as_ref().is_err_and(|s| s.code() == Code::FailedPrecondition)).count();
        let gave_up = results.iter().filter(|r| r.as_ref().is_err_and(is_conflict)).count();
        println!("limit {limit}: joined {joined}, full {full}, gave up {gave_up}");
        assert_eq!(member_count(&ctx, &id).await, i64::from(limit), "members in the table");
        assert_eq!(joined as i32, limit - 1, "joins that were told they worked");
        assert_eq!(joined + full + gave_up, results.len(), "unexpected failures: {results:?}");
    }
    db.drop_schema().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn one_player_racing_itself_ends_up_in_one_party() {
    let Some((db, ctx)) = TestDb::open(1, TABLES).await else { return };
    let contexts = contexts(&db).await;
    let account = db.accounts[0].clone();
    let copies = vec![account.clone(); 6];
    let results = race(&contexts, &copies, |ctx, account| async move {
        let accounts = std::slice::from_ref(&account);
        transaction!(&ctx, tx, { create(&tx, GroupType::Party, "p", "", accounts).await.map(|_| ()) })
    })
    .await;
    assert!(results.iter().any(Result::is_ok));
    let parties = ids_of(&ctx, account.geid, &[GroupType::Party as i32]).await.unwrap();
    assert_eq!(parties.len(), 1, "{results:?}");
    db.drop_schema().await;
}

#[tokio::test]
async fn a_failed_change_leaves_nothing_behind() {
    let Some((db, ctx)) = TestDb::open(2, TABLES).await else { return };
    let (leader, joiner) = (&db.accounts[0], &db.accounts[1]);
    let id = new_group(&ctx, GroupType::Group, leader).await;
    let result: Result<(), Status> = transaction!(&ctx, tx, {
        join(&tx, &id, joiner).await?;
        Err(Status::cancelled("changed its mind"))
    });
    assert_eq!(result.unwrap_err().code(), Code::Cancelled);
    assert_eq!(member_count(&ctx, &id).await, 1);
    db.drop_schema().await;
}

#[tokio::test]
async fn a_change_commits_as_a_whole_and_a_full_group_refuses() {
    let Some((db, ctx)) = TestDb::open(3, TABLES).await else { return };
    let id = new_group(&ctx, GroupType::Group, &db.accounts[0]).await;
    set_limit(&ctx, &id, 2).await;
    transaction!(&ctx, tx, { join(&tx, &id, &db.accounts[1]).await }).unwrap();
    let again = transaction!(&ctx, tx, { join(&tx, &id, &db.accounts[1]).await });
    assert_eq!(again.unwrap_err().code(), Code::AlreadyExists);
    let full = transaction!(&ctx, tx, { join(&tx, &id, &db.accounts[2]).await });
    assert_eq!(full.unwrap_err().code(), Code::FailedPrecondition);
    assert_eq!(member_count(&ctx, &id).await, 2);
    db.drop_schema().await;
}

/// The change that ends a group records the cleanup for every service, and draining hands each service
/// the group and its teams. Another group's rows stay.
#[tokio::test]
async fn a_destroyed_group_has_every_service_delete_its_rows_and_those_of_its_teams() {
    let Some((db, _)) = TestDb::open(2, TABLES).await else { return };
    let (fakes, peers) = group_data_services().await;
    let ctx = db.context_with(TABLES, &peers).await;
    let leader = std::slice::from_ref(&db.accounts[0]);
    let id = new_group(&ctx, GroupType::CustomLobby, &db.accounts[0]).await;
    let other = new_group(&ctx, GroupType::Group, &db.accounts[1]).await;
    let parent = urn::group(&id);
    let team =
        transaction!(&ctx, tx, { create(&tx, GroupType::CustomLobbyTeam, "t", &parent, leader).await }).unwrap().id;
    transaction!(&ctx, tx, {
        let group = lock(&tx, &id).await?;
        destroy(&tx, &group).await
    })
    .unwrap();
    let mut targets: Vec<String> = TARGETS.iter().map(|t| t.to_string()).collect();
    targets.sort();
    assert_eq!(pending_cleanups(&ctx, &id).await, targets);
    assert_eq!(pending_cleanups(&ctx, &team).await, targets);
    assert!(pending_cleanups(&ctx, &other).await.is_empty());

    assert_eq!(cleanup::drain(&ctx).await.unwrap(), 0);
    let mut expected = vec![id.clone(), team.clone()];
    expected.sort();
    for fake in &fakes {
        assert_eq!(fake.deleted(), expected);
    }
    assert!(pending_cleanups(&ctx, &id).await.is_empty() && pending_cleanups(&ctx, &team).await.is_empty());
    assert_eq!(load(&ctx, &id).await.unwrap_err().code(), Code::NotFound);
    assert!(load(&ctx, &other).await.is_ok());
    db.drop_schema().await;
}

/// A service that is away keeps its share recorded, and gets it once it answers. The others are not
/// asked again.
#[tokio::test]
async fn a_service_that_is_away_is_asked_again_later() {
    let Some((db, _)) = TestDb::open(1, TABLES).await else { return };
    let (fakes, peers) = group_data_services().await;
    let ctx = db.context_with(TABLES, &peers).await;
    let account = &db.accounts[0];
    let id = new_group(&ctx, GroupType::Group, account).await;
    fakes[0].failing.store(true, std::sync::atomic::Ordering::Relaxed);
    transaction!(&ctx, tx, {
        let group = lock(&tx, &id).await?;
        leave(&tx, &group, &member_of_account(account), None).await
    })
    .unwrap();

    assert_eq!(cleanup::drain(&ctx).await.unwrap(), 1);
    assert_eq!(pending_cleanups(&ctx, &id).await, ["chat"]);
    assert!(fakes[0].deleted().is_empty());
    fakes[1..].iter().for_each(|fake| assert_eq!(fake.deleted(), [id.clone()]));

    fakes[0].failing.store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(cleanup::drain(&ctx).await.unwrap(), 0);
    assert_eq!(fakes[0].deleted(), [id.clone()]);
    fakes[1..].iter().for_each(|fake| assert_eq!(fake.deleted(), [id.clone()], "asked once"));
    assert!(pending_cleanups(&ctx, &id).await.is_empty());
    db.drop_schema().await;
}

/// The worker runs as soon as the destroying change commits, not at its next retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_worker_cleans_up_right_after_the_commit() {
    let Some((db, _)) = TestDb::open(1, TABLES).await else { return };
    let (fakes, peers) = group_data_services().await;
    let ctx = Arc::new(db.context_with(TABLES, &peers).await);
    let worker = ctx.clone();
    let running = tokio::spawn(async move { cleanup::run(&worker).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let id = new_group(&ctx, GroupType::Group, &db.accounts[0]).await;
    transaction!(&ctx, tx, {
        let group = lock(&tx, &id).await?;
        destroy(&tx, &group).await
    })
    .unwrap();
    for _ in 0..100 {
        if fakes.iter().all(|fake| fake.deleted() == [id.clone()]) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    running.abort();
    fakes.iter().for_each(|fake| assert_eq!(fake.deleted(), [id.clone()]));
    db.drop_schema().await;
}

/// SocialInternal over the network, through the clients the other services use.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn other_services_read_and_change_groups_through_social_internal() {
    let Some((db, _)) = TestDb::open(3, TABLES).await else { return };
    let (fakes, peers) = group_data_services().await;
    let social = Arc::new(Social { ctx: db.context_with(TABLES, &peers).await });
    let address = serve(Routes::new(SocialInternalServer::from_arc(social.clone()))).await;
    let client = Peers::connect(&PeerArgs { social: address, ..nowhere() }).unwrap();
    let (one, two, three) = (&db.accounts[0], &db.accounts[1], &db.accounts[2]);

    let squad = client.create_group(GroupType::Squad, "sq", "", vec![one.geid, two.geid]).await.unwrap();
    assert_eq!(squad.group.r#type, GroupType::Squad as i32);
    assert!(sc_social::groups::is_leader(&squad.group, one.geid));
    let accounts: Vec<(u64, u32)> = squad.members.iter().map(|m| (m.geid, m.account_id)).collect();
    assert_eq!(accounts, [(one.geid, one.account_id), (two.geid, two.account_id)]);
    let unknown = client.create_group(GroupType::Squad, "", "", vec![1]).await.unwrap_err();
    assert_eq!(unknown.code(), Code::NotFound);

    let lobby = client.create_group(GroupType::CustomLobby, "", "", vec![one.geid]).await.unwrap().group.id;
    assert_eq!(client.group_ids(one.geid, &[]).await.unwrap(), [squad.group.id.clone(), lobby.clone()]);
    assert_eq!(client.group_ids(one.geid, &[GroupType::CustomLobby as i32]).await.unwrap(), [lobby.clone()]);
    let found = client.groups(&[lobby.clone(), "missing".to_string(), squad.group.id.clone()]).await.unwrap();
    let ids: Vec<&str> = found.iter().map(|view| view.group.id.as_str()).collect();
    assert_eq!(ids, [lobby.as_str(), squad.group.id.as_str()], "in order, without what is missing");
    assert!(client.exists(&lobby).await.unwrap() && !client.exists("missing").await.unwrap());
    assert_eq!(client.member_of(&lobby, two.geid).await.unwrap_err().code(), Code::PermissionDenied);

    client.sync_teams(&lobby, 2, GroupType::CustomLobbyTeam).await.unwrap();
    let teams = client.teams(&lobby).await.unwrap();
    let names: Vec<&str> = teams.iter().map(|team| team.group.name.as_str()).collect();
    assert_eq!(names, ["Team 1", "Team 2"]);
    client.join_group(&teams[0].group.id, one.geid).await.unwrap();
    client.join_group(&lobby, two.geid).await.unwrap();
    let again = client.join_group(&lobby, two.geid).await.unwrap_err();
    assert_eq!(again.code(), Code::AlreadyExists);
    let teams = client.teams(&lobby).await.unwrap();
    assert!(is_member(&teams[1].group, two.geid), "a joining player gets the emptiest team");

    let (view, previous) = client.move_to_team(&teams[0].group.id, two.geid).await.unwrap();
    assert_eq!((view.group.id.as_str(), previous.as_str()), (lobby.as_str(), teams[1].group.id.as_str()));
    let (_, stayed) = client.move_to_team(&teams[0].group.id, two.geid).await.unwrap();
    assert_eq!(stayed, teams[0].group.id, "already there");
    let outsider = client.move_to_team(&teams[0].group.id, three.geid).await.unwrap_err();
    assert_eq!(outsider.code(), Code::PermissionDenied);
    let not_a_team = client.move_to_team(&lobby, one.geid).await.unwrap_err();
    assert_eq!(not_a_team.code(), Code::InvalidArgument);

    client.move_to_team(&teams[1].group.id, two.geid).await.unwrap();
    client.sync_teams(&lobby, 1, GroupType::CustomLobbyTeam).await.unwrap();
    let left = client.teams(&lobby).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].members.len(), 2, "the players of the team that went moved to the first");
    assert_eq!(pending_cleanups(&social.ctx, &teams[1].group.id).await.len(), TARGETS.len());

    client.destroy_group(&lobby).await.unwrap();
    assert!(client.groups(&[lobby.clone()]).await.unwrap().is_empty());
    assert!(client.teams(&lobby).await.unwrap().is_empty());
    assert_eq!(cleanup::drain(&social.ctx).await.unwrap(), 0);
    let mut expected = vec![lobby.clone(), teams[0].group.id.clone(), teams[1].group.id.clone()];
    expected.sort();
    fakes.iter().for_each(|fake| assert_eq!(fake.deleted(), expected));
    let gone = client.destroy_group(&lobby).await.unwrap_err();
    assert_eq!(gone.code(), Code::NotFound);
    db.drop_schema().await;
}

/// The handlers themselves, for what the clients above do not reach.
#[tokio::test]
async fn internal_requests_are_checked() {
    let Some((db, ctx)) = TestDb::open(1, TABLES).await else { return };
    let social = Social { ctx };
    let geid = db.accounts[0].geid;
    let bad_type = CreateGroupRequest { r#type: 9999, member_geids: vec![geid], ..Default::default() };
    assert_eq!(SocialInternal::create_group(&social, Request::new(bad_type)).await.unwrap_err().code(), Code::InvalidArgument);
    let created = CreateGroupRequest { r#type: GroupType::Group as i32, member_geids: vec![geid], ..Default::default() };
    let id = SocialInternal::create_group(&social, Request::new(created)).await.unwrap().into_inner().group.unwrap().group.unwrap().id;
    let none = SyncTeamsRequest { group_id: id.clone(), count: 0, team_type: GroupType::Team as i32 };
    assert_eq!(social.sync_teams(Request::new(none)).await.unwrap_err().code(), Code::InvalidArgument);
    let listed = social.list_group_ids(Request::new(ListGroupIdsRequest { geid, types: vec![] })).await.unwrap();
    assert_eq!(listed.into_inner().group_ids, [id.clone()]);
    let teams = social.get_teams(Request::new(GetTeamsRequest { group_id: id.clone() })).await.unwrap();
    assert!(teams.into_inner().groups.is_empty());
    let stranger = JoinGroupRequest { group_id: id.clone(), geid: 1 };
    assert_eq!(social.join_group(Request::new(stranger)).await.unwrap_err().code(), Code::NotFound);
    let nowhere = MoveToTeamRequest { team_id: "missing".to_string(), geid };
    assert_eq!(social.move_to_team(Request::new(nowhere)).await.unwrap_err().code(), Code::NotFound);
    SocialInternal::destroy_group(&social, Request::new(DestroyGroupRequest { group_id: id.clone() })).await.unwrap();
    let found = social.get_groups(Request::new(GetGroupsRequest { group_ids: vec![id] })).await.unwrap();
    assert!(found.into_inner().groups.is_empty());
    db.drop_schema().await;
}
