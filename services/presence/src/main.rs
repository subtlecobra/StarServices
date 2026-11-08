//! sc.external.services.presence.v1.PresenceService. A client holds one stream open for as long as it
//! runs: what it sends is its presence, and the end of the stream takes it offline. Friends hear of a
//! change on `account/<id>/presence` and fellow group members on the group's topic.
//!
//! sc-presence owns `player_presence`: sc-social and sc-contacts ask it through
//! scbackend.internal.PresenceInternal, served on the same port. It asks sc-contacts for friends and
//! sc-social for groups.

use std::{
    net::SocketAddr,
    process::ExitCode,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use clap::Parser;
use futures_util::StreamExt;
use sc_db::{internal, DatabaseArgs, Row};
use sc_api::internal::{ presence_internal_server::{PresenceInternal, PresenceInternalServer}, GetPresencesRequest, GetPresencesResponse, PlayerPresence, };
use sc_proto::{
    services::presence::v1::{
    presence_command::Command,
    presence_service_server::{PresenceService, PresenceServiceServer},
    Activity, Presence, PresenceStreamRequest, PresenceStreamResponse, PresenceUpdated, SetPresenceCommand,
    Status as PresenceStatus,
    },
};
use sc_runtime::{state::Account, CommonArgs, MAX_MESSAGE_BYTES};
use sc_social::{
    groups, now_ms,
    peers::PeerArgs,
    presence::{visible, STALE_AFTER_MS},
    push::{presence_topic, Event, PRESENCE},
    Context,
};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status, Streaming};
use tracing::{info, warn};

const HEARTBEAT: Duration = Duration::from_millis(STALE_AFTER_MS as u64 / 3);

/// sc-presence's table, which it alone reads and writes.
const TABLES: &str = "
CREATE TABLE IF NOT EXISTS player_presence (
    account_id BIGINT PRIMARY KEY,
    status INT NOT NULL,
    activity_state TEXT NOT NULL DEFAULT '',
    activity_details TEXT NOT NULL DEFAULT '',
    environment TEXT NOT NULL DEFAULT '',
    stream_id TEXT NOT NULL DEFAULT '',
    updated_at BIGINT NOT NULL
);";

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

struct Presences {
    ctx: Context,
    next_stream: AtomicU64,
}

/// What others are told. The client finds the member by `activity.player_id`, so that is always set.
fn public_update(account: &Account, presence: &Presence) -> PresenceUpdated {
    let status = visible(presence.status, 0, 0);
    let activity = presence.activity.clone().filter(|_| status != PresenceStatus::Offline).unwrap_or_default();
    PresenceUpdated {
        presence: Some(Presence {
            status: status as i32,
            activity: Some(Activity { player_id: account.geid, ..activity }),
        }),
        account_id: account.account_id,
    }
}

/// A row as anyone but the player sees it: invisible or stale is offline, without activity or environment.
fn seen(row: &Row, now: i64) -> PlayerPresence {
    let status = visible(row.get("status"), row.get("updated_at"), now);
    let online = status != PresenceStatus::Offline;
    let activity =
        online.then(|| Activity { state: row.get("activity_state"), details: row.get("activity_details"), player_id: 0 });
    PlayerPresence {
        account_id: row.get::<_, i64>("account_id") as u32,
        presence: Some(Presence { status: status as i32, activity }),
        environment: if online { row.get("environment") } else { String::new() },
    }
}

impl Presences {
    async fn store(&self, account: &Account, stream: &str, presence: &Presence, environment: &str) -> Result<(), Status> {
        let activity = presence.activity.clone().unwrap_or_default();
        self.ctx
            .db
            .execute(
                "INSERT INTO player_presence
                     (account_id, status, activity_state, activity_details, environment, stream_id, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (account_id) DO UPDATE SET status = excluded.status,
                     activity_state = excluded.activity_state, activity_details = excluded.activity_details,
                     environment = excluded.environment, stream_id = excluded.stream_id,
                     updated_at = excluded.updated_at",
                &[
                    &i64::from(account.account_id),
                    &presence.status,
                    &activity.state,
                    &activity.details,
                    &environment,
                    &stream,
                    &now_ms(),
                ],
            )
            .await
            .map_err(internal)?;
        Ok(())
    }

    /// Tells friends, from sc-contacts, and fellow members of groups that follow presence, from
    /// sc-social.
    async fn announce(&self, account: &Account, presence: &Presence) -> Result<(), Status> {
        let ctx = &self.ctx;
        let event = Event::new(PRESENCE, "PresenceUpdated", &public_update(account, presence));
        for friend in ctx.peers.friend_ids(account.account_id).await? {
            ctx.push.send(friend, presence_topic(account.account_id), &event);
        }
        for view in ctx.peers.groups(&ctx.peers.group_ids(account.geid, &[]).await?).await? {
            if view.group.settings.as_ref().is_some_and(|s| s.presence_aware) {
                let mut members = view.members;
                members.retain(|member| member.geid != account.geid);
                groups::to_members(ctx, &view.group.id, &members, &event);
            }
        }
        Ok(())
    }

    /// The presence is stored either way: a friend or group that could not be told reads it back.
    async fn announce_or_warn(&self, account: &Account, presence: &Presence) {
        if let Err(error) = self.announce(account, presence).await {
            warn!(handle = %account.handle, %error, "presence not announced");
        }
    }

    async fn set(&self, account: &Account, stream: &str, command: SetPresenceCommand) -> Result<(), Status> {
        let presence = command.presence.unwrap_or_default();
        self.store(account, stream, &presence, &command.environment).await?;
        self.announce_or_warn(account, &presence).await;
        Ok(())
    }

    async fn refresh(&self, account: &Account, stream: &str) -> Result<(), Status> {
        self.ctx
            .db
            .execute(
                "UPDATE player_presence SET updated_at = $3 WHERE account_id = $1 AND stream_id = $2",
                &[&i64::from(account.account_id), &stream, &now_ms()],
            )
            .await
            .map_err(internal)?;
        Ok(())
    }

    /// Goes offline, unless a newer stream took over: a client that reconnects can open the new
    /// stream before the old one is seen to end.
    async fn close(&self, account: &Account, stream: &str) -> Result<(), Status> {
        let offline = Presence { status: PresenceStatus::Offline as i32, activity: None };
        let closed = self
            .ctx
            .db
            .execute(
                "UPDATE player_presence SET status = $3, activity_state = '', activity_details = '',
                     environment = '', updated_at = $4
                 WHERE account_id = $1 AND stream_id = $2",
                &[&i64::from(account.account_id), &stream, &offline.status, &now_ms()],
            )
            .await
            .map_err(internal)?;
        if closed > 0 {
            self.announce_or_warn(account, &offline).await;
        }
        Ok(())
    }

    async fn follow(
        &self,
        account: &Account,
        id: &str,
        stream: &mut Streaming<PresenceStreamRequest>,
    ) -> Result<(), Status> {
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        loop {
            tokio::select! {
                message = stream.next() => match message {
                    Some(Ok(request)) => {
                        let command = request.command.and_then(|c| c.command);
                        if let Some(Command::SetPresence(set)) = command {
                            self.set(account, id, set).await?;
                        }
                    }
                    Some(Err(error)) => {
                        info!(handle = %account.handle, %error, "presence stream broke");
                        return Ok(());
                    }
                    None => return Ok(()),
                },
                _ = heartbeat.tick() => self.refresh(account, id).await?,
            }
        }
    }
}

#[tonic::async_trait]
impl PresenceService for Presences {
    async fn presence_stream(
        &self,
        request: Request<Streaming<PresenceStreamRequest>>,
    ) -> Result<Response<PresenceStreamResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let mut stream = request.into_inner();
        let id = format!("{}-{}", now_ms(), self.next_stream.fetch_add(1, Ordering::Relaxed));
        let online = Presence { status: PresenceStatus::Online as i32, activity: None };
        self.set(&account, &id, SetPresenceCommand { presence: Some(online), environment: String::new() }).await?;
        info!(handle = %account.handle, "presence stream opened");
        let result = self.follow(&account, &id, &mut stream).await;
        if let Err(error) = self.close(&account, &id).await {
            warn!(handle = %account.handle, %error, "presence not cleared");
        }
        info!(handle = %account.handle, "presence stream closed");
        result.map(|()| Response::new(PresenceStreamResponse {}))
    }
}

/// What the other services ask of presence. No account is checked: the caller is a backend service.
#[tonic::async_trait]
impl PresenceInternal for Presences {
    async fn get_presences(
        &self,
        request: Request<GetPresencesRequest>,
    ) -> Result<Response<GetPresencesResponse>, Status> {
        let ids: Vec<i64> = request.into_inner().account_ids.into_iter().map(i64::from).collect();
        let rows = self
            .ctx
            .db
            .query(
                "SELECT account_id, status, activity_state, activity_details, environment, updated_at
                 FROM player_presence WHERE account_id = ANY($1)",
                &[&ids],
            )
            .await
            .map_err(internal)?;
        let now = now_ms();
        Ok(Response::new(GetPresencesResponse { presences: rows.iter().map(|row| seen(row, now)).collect() }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let ctx = Context::open(&args.common, &args.database, &args.push, &args.peers, TABLES).await?;
    let service = Arc::new(Presences { ctx, next_stream: AtomicU64::new(0) });
    let server = PresenceServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    let internal = PresenceInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, push = %args.push, "presence listening");
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

    async fn presences_of(service: &Presences, ids: &[u32]) -> Vec<PlayerPresence> {
        let request = Request::new(GetPresencesRequest { account_ids: ids.to_vec() });
        let mut found = service.get_presences(request).await.unwrap().into_inner().presences;
        found.sort_by_key(|p| p.account_id);
        found
    }

    /// What the other services read: as others see it, and nothing for an account never online. The
    /// friends and groups that sc-contacts and sc-social would name are away, which stores the
    /// presence all the same.
    #[tokio::test]
    async fn others_see_presence_as_friends_would() {
        let Some((db, ctx)) = TestDb::open(3, TABLES).await else { return };
        let service = Presences { ctx, next_stream: AtomicU64::new(0) };
        let (one, two, three) = (&db.accounts[0], &db.accounts[1], &db.accounts[2]);
        let activity = Activity { state: "flying".to_string(), details: "Stanton".to_string(), player_id: 0 };
        let set = |status: PresenceStatus| SetPresenceCommand {
            presence: Some(Presence { status: status as i32, activity: Some(activity.clone()) }),
            environment: "pu".to_string(),
        };
        service.set(one, "s1", set(PresenceStatus::Online)).await.unwrap();
        service.set(two, "s2", set(PresenceStatus::Invisible)).await.unwrap();

        let found = presences_of(&service, &[one.account_id, two.account_id, three.account_id]).await;
        assert_eq!(found.len(), 2, "never online is left out");
        let online = found[0].presence.clone().unwrap();
        assert_eq!((found[0].account_id, online.status), (one.account_id, PresenceStatus::Online as i32));
        assert_eq!((online.activity.unwrap().details.as_str(), found[0].environment.as_str()), ("Stanton", "pu"));
        let hidden = found[1].presence.clone().unwrap();
        assert_eq!(hidden.status, PresenceStatus::Offline as i32);
        assert!(hidden.activity.is_none() && found[1].environment.is_empty(), "invisible gives nothing away");

        service.close(one, "s1").await.unwrap();
        let closed = presences_of(&service, &[one.account_id]).await;
        assert_eq!(closed[0].presence.as_ref().unwrap().status, PresenceStatus::Offline as i32);
        db.drop_schema().await;
    }

    fn account() -> Account {
        Account { account_id: 7, handle: "Pilot".to_string(), geid: 300, created_at_ms: 0 }
    }

    #[test]
    fn an_update_names_the_player() {
        let activity = Activity { state: "flying".to_string(), details: "Stanton".to_string(), player_id: 0 };
        let presence = Presence { status: PresenceStatus::Online as i32, activity: Some(activity) };
        let update = public_update(&account(), &presence);
        let shown = update.presence.unwrap();
        assert_eq!(update.account_id, 7);
        assert_eq!(shown.status, PresenceStatus::Online as i32);
        assert_eq!(shown.activity.as_ref().unwrap().player_id, 300);
        assert_eq!(shown.activity.unwrap().state, "flying");
    }

    #[test]
    fn invisible_shows_as_offline_without_activity() {
        let activity = Activity { state: "hiding".to_string(), ..Default::default() };
        let presence = Presence { status: PresenceStatus::Invisible as i32, activity: Some(activity) };
        let shown = public_update(&account(), &presence).presence.unwrap();
        assert_eq!(shown.status, PresenceStatus::Offline as i32);
        assert_eq!(shown.activity.unwrap().state, "");
    }
}
