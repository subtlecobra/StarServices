//! Where a player is, against a real database. It needs `SCBACKEND_TEST_POSTGRES`, see `testing`. The
//! group store is sc-social's binary, and its tests are there (`src/store/tests.rs`).

use sc_api::internal::PlayerPresence;
use sc_proto::{
    services::presence::v1::{Activity, Presence, Status as PresenceStatus},
};
use sc_runtime::state::Account;
use sc_social::{
    peers::PeerArgs,
    presence::{self, visible},
    testing::{nowhere, FakePresence, TestDb},
};
use tonic::{Code, Status};

/// sc-presence owns `player_presence`: the stand-in answers as it would for this status, in the menu at Stanton.
fn set_presence(presences: &FakePresence, account: &Account, status: PresenceStatus) {
    let status = visible(status as i32, 0, 0);
    let online = status != PresenceStatus::Offline;
    let activity = online.then(|| Activity { state: String::new(), details: "Stanton".to_string(), player_id: 0 });
    let environment = if online { "menu" } else { "" }.to_string();
    let presence = Some(Presence { status: status as i32, activity });
    presences.set(PlayerPresence { account_id: account.account_id, presence, environment });
}

/// sc-hub owns `player_shards`: a stand-in answers `HubInternal` with `shard_of` on `shard-a` at location 5, beside
/// one other player at another location.
async fn fake_hub(shard_of: u64) -> String {
    use sc_api::internal::{
        hub_internal_server::{HubInternal, HubInternalServer},
        GetPlayerShardRequest, GetPlayerShardResponse, PlayerShard,
    };

    struct FakeHub(u64);

    #[tonic::async_trait]
    impl HubInternal for FakeHub {
        async fn get_player_shard(
            &self,
            request: tonic::Request<GetPlayerShardRequest>,
        ) -> Result<tonic::Response<GetPlayerShardResponse>, Status> {
            let shard = (request.into_inner().geid == self.0).then(|| PlayerShard {
                shard_id: "shard-a".into(),
                public_address: "10.0.0.1".into(),
                public_port: 7000,
                location_id: 5,
                players_at_location: 1,
                players_on_shard: 2,
            });
            Ok(tonic::Response::new(GetPlayerShardResponse { shard }))
        }

        async fn resolve_session(
            &self,
            _: tonic::Request<sc_api::internal::ResolveSessionRequest>,
        ) -> Result<tonic::Response<sc_api::internal::ResolveSessionResponse>, Status> {
            Ok(tonic::Response::new(sc_api::internal::ResolveSessionResponse::default()))
        }

        async fn report_disconnect(
            &self,
            _: tonic::Request<sc_api::internal::PlayerDisconnect>,
        ) -> Result<tonic::Response<sc_api::internal::ReportDisconnectResponse>, Status> {
            Err(Status::unimplemented("report_disconnect"))
        }
    }

    let address = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    tokio::spawn(tonic::transport::Server::builder().add_service(HubInternalServer::new(FakeHub(shard_of))).serve(address));
    while tokio::net::TcpStream::connect(address).await.is_err() {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    format!("http://{address}")
}

#[tokio::test]
async fn shard_info_comes_from_the_hub() {
    let Some((db, _)) = TestDb::open(3, "").await else { return };
    let (one, other, third) = (&db.accounts[0], &db.accounts[1], &db.accounts[2]);
    presence::use_hub(&fake_hub(one.geid).await).unwrap();
    let presences = FakePresence::default();
    let ctx = db.context_with("", &PeerArgs { presence: presences.serve().await, ..nowhere() }).await;
    set_presence(&presences, one, PresenceStatus::Online);
    let info = presence::shard_info(&ctx, one).await.unwrap();
    assert_eq!((info.id.as_str(), info.dgs_id.as_str()), ("shard-a", "10.0.0.1:7000"));
    assert_eq!((info.location_id, info.location.as_str()), (5, "Stanton"));
    assert_eq!((info.player_count, info.total_players), (1, 2));

    set_presence(&presences, one, PresenceStatus::Invisible);
    assert_eq!(presence::shard_info(&ctx, one).await.unwrap_err().code(), Code::NotFound);
    assert_eq!(presence::shard_info(&ctx, other).await.unwrap_err().code(), Code::NotFound, "no presence row");

    set_presence(&presences, third, PresenceStatus::Online);
    let fallback = presence::shard_info(&ctx, third).await.unwrap();
    assert_eq!((fallback.id.as_str(), fallback.location.as_str(), fallback.location_id), ("menu", "Stanton", 0));
    assert_eq!(fallback.dgs_id, "", "on no shard");
    db.drop_schema().await;
}
