//! What other players see of a player's presence. sc-presence owns `player_presence`: the others get
//! a presence from it through [`Peers::presences`](crate::peers::Peers::presences).

use std::{sync::OnceLock, time::Duration};

use sc_api::internal::{hub_internal_client::HubInternalClient, GetPlayerShardRequest, PlayerShard};
use sc_proto::{
    common::shard::v1::ShardInfo,
    services::presence::v1::{Presence, Status as PresenceStatus},
};
use sc_runtime::state::Account;
use tonic::{
    transport::{Channel, Endpoint},
    Request, Status,
};
use tracing::warn;

use crate::Context;

/// An open stream refreshes its row more often than this, so an older row outlived its service.
pub const STALE_AFTER_MS: i64 = 180_000;

/// What others see: an invisible player, or one whose row went stale, is offline.
pub fn visible(status: i32, updated_at: i64, now: i64) -> PresenceStatus {
    let status = PresenceStatus::try_from(status).unwrap_or(PresenceStatus::Offline);
    let stale = now - updated_at > STALE_AFTER_MS;
    match status {
        PresenceStatus::Invisible | PresenceStatus::Unspecified => PresenceStatus::Offline,
        _ if stale => PresenceStatus::Offline,
        status => status,
    }
}

pub fn offline() -> Presence {
    Presence { status: PresenceStatus::Offline as i32, activity: None }
}

/// Where sc-hub, which owns `player_shards`, answers `HubInternal`. Read from this variable on first use.
pub const HUB_ENV: &str = "SCBACKEND_HUB";
pub const HUB: &str = "http://sc-hub-internal:50051";
const HUB_TIMEOUT: Duration = Duration::from_secs(3);
const HUB_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

static HUB_CLIENT: OnceLock<HubInternalClient<Channel>> = OnceLock::new();

/// Sets the Hub's address before the first use, as tests do. Later calls change nothing.
pub fn use_hub(address: &str) -> Result<(), String> {
    let endpoint = Endpoint::from_shared(address.to_string()).map_err(|e| format!("hub address {address}: {e}"))?;
    let client = HubInternalClient::new(endpoint.connect_timeout(HUB_CONNECT_TIMEOUT).connect_lazy());
    let _ = HUB_CLIENT.set(client);
    Ok(())
}

fn hub() -> Option<HubInternalClient<Channel>> {
    if HUB_CLIENT.get().is_none() {
        let address = std::env::var(HUB_ENV).unwrap_or_else(|_| HUB.to_string());
        if let Err(error) = use_hub(&address) {
            warn!(%error, "no hub");
        }
    }
    HUB_CLIENT.get().cloned()
}

/// The shard the Hub sent this player to. None for a player on no shard, and when the Hub does not answer, which
/// shows the player as if it were on none.
pub async fn player_shard(geid: u64) -> Option<PlayerShard> {
    let mut request = Request::new(GetPlayerShardRequest { geid });
    request.set_timeout(HUB_TIMEOUT);
    match hub()?.get_player_shard(request).await {
        Ok(reply) => reply.into_inner().shard,
        Err(status) => {
            warn!(%status, "hub did not say where the player is");
            None
        }
    }
}

fn on_shard(shard: PlayerShard, location: String) -> ShardInfo {
    let dgs_id = if shard.public_address.is_empty() {
        String::new()
    } else {
        format!("{}:{}", shard.public_address, shard.public_port)
    };
    ShardInfo {
        id: shard.shard_id,
        dgs_id,
        location_id: shard.location_id,
        location,
        player_count: shard.players_at_location as i32,
        total_players: shard.players_on_shard as i32,
    }
}

/// Where a player is, as sc-hub recorded it. Presence decides first, so a shard row cannot give away an
/// invisible player. A player online but on no shard, such as in the menus, shows what the client
/// reported about itself.
pub async fn shard_info(ctx: &Context, account: &Account) -> Result<ShardInfo, Status> {
    let presence = ctx.peers.presences(&[account.account_id]).await?.remove(&account.account_id);
    let online = |p: &sc_api::internal::PlayerPresence| {
        p.presence.as_ref().is_some_and(|p| p.status != PresenceStatus::Offline as i32)
    };
    let presence = presence.filter(online).ok_or_else(|| Status::not_found(format!("{} is offline", account.handle)))?;
    let shard = player_shard(account.geid).await;
    let location = presence.presence.and_then(|p| p.activity).map(|a| a.details).unwrap_or_default();
    Ok(match shard {
        Some(shard) => on_shard(shard, location),
        None => ShardInfo { id: presence.environment, location, ..Default::default() },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invisible_and_stale_players_look_offline() {
        let online = PresenceStatus::Online as i32;
        assert_eq!(visible(online, 1000, 1000), PresenceStatus::Online);
        assert_eq!(visible(online, 0, STALE_AFTER_MS + 1), PresenceStatus::Offline);
        assert_eq!(visible(PresenceStatus::Invisible as i32, 1000, 1000), PresenceStatus::Offline);
        assert_eq!(visible(99, 1000, 1000), PresenceStatus::Offline);
    }

    #[test]
    fn a_shard_shows_its_address_and_counts() {
        let shard = PlayerShard {
            shard_id: "shard-a".into(),
            public_address: "10.0.0.1".into(),
            public_port: 7000,
            location_id: 5,
            players_at_location: 1,
            players_on_shard: 2,
        };
        let info = on_shard(shard, "Stanton".into());
        assert_eq!((info.id.as_str(), info.dgs_id.as_str()), ("shard-a", "10.0.0.1:7000"));
        assert_eq!((info.location_id, info.location.as_str(), info.player_count, info.total_players), (5, "Stanton", 1, 2));
        assert_eq!(on_shard(PlayerShard::default(), String::new()).dgs_id, "", "no address, no DGS id");
    }

    /// Puts player 1 on `shard-a`, and no one else anywhere.
    struct FakeHub;

    #[tonic::async_trait]
    impl sc_api::internal::hub_internal_server::HubInternal for FakeHub {
        async fn get_player_shard(
            &self,
            request: Request<GetPlayerShardRequest>,
        ) -> Result<tonic::Response<sc_api::internal::GetPlayerShardResponse>, Status> {
            let shard = (request.into_inner().geid == 1).then(|| PlayerShard { shard_id: "shard-a".into(), ..Default::default() });
            Ok(tonic::Response::new(sc_api::internal::GetPlayerShardResponse { shard }))
        }

        async fn resolve_session(
            &self,
            _: tonic::Request<sc_api::internal::ResolveSessionRequest>,
        ) -> Result<tonic::Response<sc_api::internal::ResolveSessionResponse>, Status> {
            Ok(tonic::Response::new(sc_api::internal::ResolveSessionResponse::default()))
        }
    }

    #[tokio::test]
    async fn the_shard_comes_from_the_hub() {
        let address = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let server = sc_api::internal::hub_internal_server::HubInternalServer::new(FakeHub);
        tokio::spawn(tonic::transport::Server::builder().add_service(server).serve(address));
        while tokio::net::TcpStream::connect(address).await.is_err() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        use_hub(&format!("http://{address}")).unwrap();
        assert_eq!(player_shard(1).await.unwrap().shard_id, "shard-a");
        assert!(player_shard(2).await.is_none());
    }
}
