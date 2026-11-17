//! Tickets for squads and lobbies, through the matchmaking service. A ticket belongs to whoever asks:
//! the leader's own token goes along, so matchmaking sees its usual caller.

use std::time::Duration;

use sc_proto::{
    common::{
        game::v1::{ArenaCommanderMode, StarMarineMode},
        network::v1::Region,
    },
    services::matchmaking::v1::{
        frontend_service_client::FrontendServiceClient, match_request, GameArenaCommanderMatchRequest,
        GameStarMarineMatchRequest, MatchRequest,
    },
};
use tonic::{
    metadata::{MetadataMap, MetadataValue},
    transport::{Channel, Endpoint},
    Request, Status,
};

use crate::modes::Mode;

const TIMEOUT: Duration = Duration::from_secs(5);
const AUTHORIZATION: &str = "authorization";

pub struct Matchmaking {
    client: FrontendServiceClient<Channel>,
}

/// The game a mode belongs to, with its mode in that game's numbering.
enum Game {
    Arena(ArenaCommanderMode),
    Marine(StarMarineMode),
}

fn game(mode: &Mode) -> Game {
    match mode.name {
        "EA_PirateSwarm" => Game::Arena(ArenaCommanderMode::PirateSwarm),
        "EA_SquadronBattle" => Game::Arena(ArenaCommanderMode::SquadronBattle),
        "EA_VanduulSwarm" => Game::Arena(ArenaCommanderMode::VanduulSwarm),
        "EA_ClassicRace" => Game::Arena(ArenaCommanderMode::ClassicRace),
        "EA_Duel" => Game::Arena(ArenaCommanderMode::Duel),
        "EA_GravRace" => Game::Arena(ArenaCommanderMode::GravRace),
        "EA_Elimination" => Game::Marine(StarMarineMode::Elimination),
        "EA_TeamElimination" => Game::Marine(StarMarineMode::TeamElimination),
        "EA_Control" => Game::Marine(StarMarineMode::Control),
        "EA_GunGame" => Game::Marine(StarMarineMode::Fpsgungame),
        _ => Game::Arena(ArenaCommanderMode::FreeFlight),
    }
}

/// A lobby names its region freely. Anything else is left for matchmaking to place.
pub fn region(name: &str) -> Region {
    match name.to_ascii_lowercase().as_str() {
        "us" | "usa" => Region::Us,
        "eu" | "europe" => Region::Europe,
        "oceania" => Region::Oceania,
        "asia" => Region::Asia,
        _ => Region::Unspecified,
    }
}

/// What a squad or lobby asks for. `map` may be empty, which leaves the choice to matchmaking.
pub fn request(group_id: &str, mode: &Mode, map: &str, region: Region) -> MatchRequest {
    let squad_id = group_id.to_string();
    let maps = if map.is_empty() { Vec::new() } else { vec![map.to_string()] };
    let game = match game(mode) {
        Game::Arena(mode) => match_request::Game::ArenaCommander(GameArenaCommanderMatchRequest {
            mode: mode as i32,
            squad_id,
            maps,
            ..Default::default()
        }),
        Game::Marine(mode) => match_request::Game::StarMarine(GameStarMarineMatchRequest {
            mode: mode as i32,
            squad_id,
            maps,
            ..Default::default()
        }),
    };
    MatchRequest { region: region as i32, game: Some(game) }
}

impl Matchmaking {
    pub fn connect(address: &str) -> Result<Self, String> {
        let endpoint = Endpoint::from_shared(address.to_string()).map_err(|e| format!("--matchmaking: {e}"))?;
        Ok(Self { client: FrontendServiceClient::new(endpoint.timeout(TIMEOUT).connect_lazy()) })
    }

    /// Opens a ticket as the player of `caller`, the metadata of its own call, and returns its id.
    pub async fn open_ticket(&self, caller: &MetadataMap, match_request: MatchRequest) -> Result<String, Status> {
        let token: &MetadataValue<_> =
            caller.get(AUTHORIZATION).ok_or_else(|| Status::unauthenticated("missing or invalid token"))?;
        let mut request = Request::new(match_request);
        request.metadata_mut().insert(AUTHORIZATION, token.clone());
        Ok(self.client.clone().r#match(request).await?.into_inner().ticket_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::{find, MODES};

    #[test]
    fn every_mode_has_its_own_game_mode() {
        let numbers: Vec<(bool, i32)> = MODES
            .iter()
            .map(|mode| match game(mode) {
                Game::Arena(mode) => (true, mode as i32),
                Game::Marine(mode) => (false, mode as i32),
            })
            .collect();
        let mut unique = numbers.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), numbers.len());
    }

    #[test]
    fn a_request_names_the_group_and_the_map() {
        let request = request("g", find("EA_Control"), "EA_Gundo", Region::Asia);
        assert_eq!(request.region, Region::Asia as i32);
        let Some(match_request::Game::StarMarine(marine)) = request.game else { panic!("wrong game") };
        assert_eq!(marine.mode, StarMarineMode::Control as i32);
        assert_eq!((marine.squad_id.as_str(), marine.maps.as_slice()), ("g", &["EA_Gundo".to_string()][..]));
    }

    #[test]
    fn regions_follow_the_shard_clusters() {
        assert_eq!(region("Europe"), Region::Europe);
        assert_eq!(region("usa"), Region::Us);
        assert_eq!(region("mars"), Region::Unspecified);
    }
}
