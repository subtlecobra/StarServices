//! sc.external.services.rtc.v1.RTCService. This keeps calls and channels. Audio needs a voice server,
//! which is not part of this backend: `--voice-host` names what the client is told to connect to.
//!
//! sc-rtc owns `rtc_channels` and `rtc_calls`. Groups are sc-social's, which it asks through
//! SocialInternal; when sc-social destroys a group it has the group's channel and calls deleted through
//! scbackend.internal.GroupDataInternal, served on the same port.

use std::{net::SocketAddr, process::ExitCode, sync::Arc};

use clap::Parser;
use sc_db::{internal, DatabaseArgs, Row};
use sc_proto::{
    internal::{
        group_data_internal_server::{GroupDataInternal, GroupDataInternalServer},
        DeleteGroupDataRequest, DeleteGroupDataResponse,
    },
    services::{
    rtc::v1::{
        rtc_service_server::{RtcService, RtcServiceServer},
        AcceptCallRequest, AcceptCallResponse, Call, CallAccepted, CallDeclined, CallHungup, CallReceived, CallRequest,
        CallResponse, Channel, DeclineCallRequest, DeclineCallResponse, GetChannelsRequest, GetChannelsResponse,
        GetPendingCallsRequest, GetPendingCallsResponse, HangupCallRequest, HangupCallResponse, Protocol,
        Server as VoiceServer,
    },
    social::v1::Group,
    },
};
use sc_runtime::{state::Account, CommonArgs, MAX_MESSAGE_BYTES};
use sc_social::{
    groups::{self, member_of_account, Member},
    now_ms,
    page::Page,
    peers::PeerArgs,
    push::{Event, RTC},
    transaction,
    tx::Queries,
    urn, Context,
};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

/// sc-rtc's tables, which it alone reads and writes.
const TABLES: &str = "
CREATE TABLE IF NOT EXISTS rtc_channels (
    group_id TEXT PRIMARY KEY,
    created_at BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS rtc_calls (
    group_id TEXT NOT NULL,
    caller_geid BIGINT NOT NULL,
    callee_geid BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (group_id, callee_geid)
);
CREATE INDEX IF NOT EXISTS rtc_calls_callee ON rtc_calls (callee_geid);";

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
    /// Host of the voice server.
    #[arg(long, env = "SCBACKEND_VOICE_HOST", default_value = "sc-voice")]
    voice_host: String,
    /// Port of the voice server.
    #[arg(long, env = "SCBACKEND_VOICE_PORT", default_value_t = 443)]
    voice_port: i32,
    #[command(flatten)]
    peers: PeerArgs,
}

struct Rtc {
    ctx: Context,
    voice: VoiceServer,
}

fn call(row: &Row) -> Call {
    Call {
        group_id: row.get("group_id"),
        caller_urn: urn::player(row.get::<_, i64>("caller_geid") as u64),
        callee_urn: urn::player(row.get::<_, i64>("callee_geid") as u64),
    }
}

fn voice_enabled(group: &Group) -> bool {
    group.settings.as_ref().and_then(|s| s.rtc.as_ref()).is_some_and(|rtc| rtc.enabled)
}

impl Rtc {
    /// The channel of a group, created the first time anyone asks for it. The token is `account`'s.
    async fn channel(&self, group: &Group, account: &Account) -> Result<Channel, Status> {
        let ctx = &self.ctx;
        let added = ctx
            .execute(
                "INSERT INTO rtc_channels (group_id, created_at) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                &[&group.id, &now_ms()],
            )
            .await?;
        self.check_group(&group.id, added).await?;
        let row = ctx.query_opt("SELECT created_at FROM rtc_channels WHERE group_id = $1", &[&group.id]).await?;
        let row = row.ok_or_else(|| Status::aborted("channel changed"))?;
        Ok(Channel {
            group_id: group.id.clone(),
            positional: group.settings.as_ref().and_then(|s| s.rtc.as_ref()).is_some_and(|rtc| rtc.positional),
            server: Some(self.voice.clone()),
            created_at: row.get("created_at"),
            token: ctx.subscription_key(account),
        })
    }

    /// Takes the call of this callee off the table.
    async fn take_call(&self, group_id: &str, account: &Account) -> Result<Call, Status> {
        let row = self
            .ctx
            .db
            .query_opt(
                "DELETE FROM rtc_calls WHERE group_id = $1 AND callee_geid = $2
                 RETURNING group_id, caller_geid, callee_geid",
                &[&group_id, &(account.geid as i64)],
            )
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found("no call on this group"))?;
        Ok(call(&row))
    }

    fn caller_of(&self, call: &Call) -> Result<Account, Status> {
        self.ctx.directory.by_urn(&call.caller_urn)
    }

    async fn delete_groups(&self, ids: &[String]) -> Result<u64, Status> {
        let calls = self.ctx.execute("DELETE FROM rtc_calls WHERE group_id = ANY($1)", &[&ids]).await?;
        let channels = self.ctx.execute("DELETE FROM rtc_channels WHERE group_id = ANY($1)", &[&ids]).await?;
        Ok(calls + channels)
    }

    /// After writing `added` rows for a group: a group destroyed since it was read has had its rows
    /// deleted already, so these go too.
    async fn check_group(&self, id: &str, added: u64) -> Result<(), Status> {
        if added > 0 && self.ctx.peers.gone(id).await {
            self.delete_groups(&[id.to_string()]).await?;
            return Err(Status::not_found(format!("no group {id}")));
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl RtcService for Rtc {
    /// Without ids, the channels of every group of the caller that has voice.
    async fn get_channels(
        &self,
        request: Request<GetChannelsRequest>,
    ) -> Result<Response<GetChannelsResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let page = Page::new(request.pagination.as_ref())?;
        let ids = if request.group_ids.is_empty() {
            ctx.peers.group_ids(account.geid, &[]).await?
        } else {
            request.group_ids
        };
        let mut groups: Vec<Group> = ctx.peers.groups(&ids).await?.into_iter().map(|view| view.group).collect();
        groups.retain(|group| groups::is_member(group, account.geid) && voice_enabled(group));
        let (groups, page_info) = page.slice(groups);
        let mut channels = Vec::with_capacity(groups.len());
        for group in &groups {
            channels.push(self.channel(group, &account).await?);
        }
        Ok(Response::new(GetChannelsResponse { channels, page_info: Some(page_info) }))
    }

    /// Rings everyone else in the group. The reply names the first of them.
    async fn call(&self, request: Request<CallRequest>) -> Result<Response<CallResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let view = ctx.peers.member_of(&request.into_inner().group_id, account.geid).await?;
        let group = &view.group;
        if !voice_enabled(group) {
            return Err(Status::failed_precondition("this group has no voice"));
        }
        let mut members: Vec<Member> = view.members.clone();
        members.retain(|member| member.geid != account.geid);
        let first = transaction!(ctx, tx, {
            let mut first = None;
            for member in &members {
                tx.execute(
                    "INSERT INTO rtc_calls (group_id, caller_geid, callee_geid, created_at) VALUES ($1, $2, $3, $4)
                     ON CONFLICT (group_id, callee_geid) DO UPDATE SET caller_geid = excluded.caller_geid,
                         created_at = excluded.created_at",
                    &[&group.id, &(account.geid as i64), &(member.geid as i64), &now_ms()],
                )
                .await?;
                let call = Call {
                    group_id: group.id.clone(),
                    caller_urn: urn::player(account.geid),
                    callee_urn: urn::player(member.geid),
                };
                let received = CallReceived { call: Some(call.clone()) };
                groups::to_player(&tx, member, &Event::new(RTC, "CallReceived", &received));
                first.get_or_insert(call);
            }
            Ok(first)
        });
        if first.as_ref().is_ok_and(Option::is_some) {
            self.check_group(&group.id, 1).await?;
        }
        let call = first?.ok_or_else(|| Status::failed_precondition("nobody else is in this group"))?;
        Ok(Response::new(CallResponse { call: Some(call) }))
    }

    async fn accept_call(&self, request: Request<AcceptCallRequest>) -> Result<Response<AcceptCallResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let group = ctx.peers.member_of(&request.into_inner().group_id, account.geid).await?.group;
        let call = self.take_call(&group.id, &account).await?;
        let caller = self.caller_of(&call)?;
        let accepted = CallAccepted { call: Some(call), channel: Some(self.channel(&group, &caller).await?) };
        groups::to_player(ctx, &member_of_account(&caller), &Event::new(RTC, "CallAccepted", &accepted));
        let channel = self.channel(&group, &account).await?;
        Ok(Response::new(AcceptCallResponse { channel: Some(channel) }))
    }

    async fn decline_call(
        &self,
        request: Request<DeclineCallRequest>,
    ) -> Result<Response<DeclineCallResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let call = self.take_call(&request.into_inner().group_id, &account).await?;
        let caller = self.caller_of(&call)?;
        let declined = CallDeclined { call: Some(call) };
        groups::to_player(ctx, &member_of_account(&caller), &Event::new(RTC, "CallDeclined", &declined));
        Ok(Response::new(DeclineCallResponse {}))
    }

    /// Ends what the caller started or was ringed for, and tells the rest of the group.
    async fn hangup_call(&self, request: Request<HangupCallRequest>) -> Result<Response<HangupCallResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let view = ctx.peers.member_of(&request.into_inner().group_id, account.geid).await?;
        let group = &view.group;
        ctx.db
            .execute(
                "DELETE FROM rtc_calls WHERE group_id = $1 AND (caller_geid = $2 OR callee_geid = $2)",
                &[&group.id, &(account.geid as i64)],
            )
            .await
            .map_err(internal)?;
        let hungup = CallHungup { group_id: group.id.clone(), member_urn: urn::player(account.geid) };
        let event = Event::new(RTC, "CallHungup", &hungup);
        for member in &view.members {
            if member.geid != account.geid {
                groups::to_player(ctx, member, &event);
            }
        }
        Ok(Response::new(HangupCallResponse {}))
    }

    async fn get_pending_calls(
        &self,
        request: Request<GetPendingCallsRequest>,
    ) -> Result<Response<GetPendingCallsResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let page = Page::new(request.get_ref().pagination.as_ref())?;
        let rows = self
            .ctx
            .db
            .query(
                "SELECT group_id, caller_geid, callee_geid FROM rtc_calls WHERE callee_geid = $1
                 ORDER BY created_at, group_id LIMIT $2 OFFSET $3",
                &[&(account.geid as i64), &page.fetch(), &page.offset],
            )
            .await
            .map_err(internal)?;
        let (rows, page_info) = page.finish(rows);
        Ok(Response::new(GetPendingCallsResponse { calls: rows.iter().map(call).collect(), page_info: Some(page_info) }))
    }
}

/// sc-social, which destroyed these groups, has their channels and calls deleted.
#[tonic::async_trait]
impl GroupDataInternal for Rtc {
    async fn delete_group_data(
        &self,
        request: Request<DeleteGroupDataRequest>,
    ) -> Result<Response<DeleteGroupDataResponse>, Status> {
        let deleted = self.delete_groups(&request.into_inner().group_ids).await?;
        Ok(Response::new(DeleteGroupDataResponse { deleted }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let voice = VoiceServer { host: args.voice_host, port: args.voice_port, protocol: Protocol::Wss as i32 };
    let ctx = Context::open(&args.common, &args.database, &args.push, &args.peers, TABLES).await?;
    let service = Arc::new(Rtc { ctx, voice });
    let server = RtcServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    let internal = GroupDataInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, push = %args.push, "rtc listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_proto::services::social::v1::{GroupSettings, RtcSettings};
    use std::time::Duration;

    use sc_proto::services::social::v1::GroupType;
    use sc_social::testing::{nowhere, serve, SocialProcess, TestDb, NOWHERE};
    use tonic::{service::Routes, Code};

    fn signed<T>(message: T, token: &str) -> Request<T> {
        let mut request = Request::new(message);
        request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
        request
    }

    fn voice() -> VoiceServer {
        VoiceServer { host: NOWHERE.to_string(), port: 1, protocol: Protocol::Wss as i32 }
    }

    async fn rows(rtc: &Rtc, group: &str) -> i64 {
        let mut total = 0;
        for table in ["rtc_channels", "rtc_calls"] {
            let sql = format!("SELECT count(*) FROM {table} WHERE group_id = $1");
            total += rtc.ctx.query_opt(&sql, &[&group]).await.unwrap().unwrap().get::<_, i64>(0);
        }
        total
    }

    #[tokio::test]
    async fn deleting_a_groups_data_takes_its_channel_and_calls() {
        let Some((db, ctx)) = TestDb::open(0, TABLES).await else { return };
        let rtc = Rtc { ctx, voice: voice() };
        for group in ["a", "b"] {
            rtc.ctx.execute("INSERT INTO rtc_channels (group_id, created_at) VALUES ($1, 0)", &[&group]).await.unwrap();
            rtc.ctx
                .execute("INSERT INTO rtc_calls (group_id, caller_geid, callee_geid, created_at) VALUES ($1, 1, 2, 0)", &[&group])
                .await
                .unwrap();
        }
        let request = Request::new(DeleteGroupDataRequest { group_ids: vec!["a".to_string()] });
        assert_eq!(rtc.delete_group_data(request).await.unwrap().into_inner().deleted, 2);
        assert_eq!((rows(&rtc, "a").await, rows(&rtc, "b").await), (0, 2));
        db.drop_schema().await;
    }

    /// The real sc-social: calls and channels go by its members, and go with the group it destroys.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn calls_and_channels_follow_the_group_that_sc_social_keeps() {
        let Some((db, _)) = TestDb::open(2, TABLES).await else { return };
        let owner = Arc::new(Rtc { ctx: db.context(TABLES).await, voice: voice() });
        let address = serve(Routes::new(GroupDataInternalServer::from_arc(owner))).await;
        let peers = sc_social::peers::PeerArgs { group_data: format!("rtc={address}"), ..nowhere() };
        let Some(social) = SocialProcess::spawn(&db, &peers).await else { return db.drop_schema().await };
        let rtc = Rtc { ctx: db.context_with(TABLES, &social.peers()).await, voice: voice() };
        let (one, two) = (&db.accounts[0], &db.accounts[1]);
        let (token_one, token_two) = (rtc.ctx.subscription_key(one), rtc.ctx.subscription_key(two));
        let group = rtc.ctx.peers.create_group(GroupType::Squad, "", "", vec![one.geid, two.geid]).await.unwrap().group.id;

        let call = rtc.call(signed(CallRequest { group_id: group.clone() }, &token_one)).await.unwrap().into_inner();
        assert_eq!(call.call.unwrap().callee_urn, urn::player(two.geid));
        let pending = rtc.get_pending_calls(signed(GetPendingCallsRequest::default(), &token_two)).await.unwrap();
        assert_eq!(pending.into_inner().calls.len(), 1);
        let channels = rtc.get_channels(signed(GetChannelsRequest::default(), &token_one)).await.unwrap();
        let channels = channels.into_inner().channels;
        assert_eq!(channels.iter().map(|c| c.group_id.as_str()).collect::<Vec<_>>(), [group.as_str()]);
        assert_eq!(rows(&rtc, &group).await, 2);

        rtc.ctx.peers.destroy_group(&group).await.unwrap();
        for _ in 0..100 {
            if rows(&rtc, &group).await == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(rows(&rtc, &group).await, 0, "sc-social had the channel and calls deleted");
        let gone = rtc.call(signed(CallRequest { group_id: group.clone() }, &token_one)).await.unwrap_err();
        assert_eq!(gone.code(), Code::NotFound);
        drop(social);
        db.drop_schema().await;
    }

    #[test]
    fn voice_follows_the_group_settings() {
        let settings = GroupSettings { rtc: Some(RtcSettings { enabled: true, positional: false }), ..Default::default() };
        assert!(voice_enabled(&Group { settings: Some(settings), ..Default::default() }));
        assert!(!voice_enabled(&Group::default()));
    }
}
