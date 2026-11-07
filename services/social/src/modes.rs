//! Arena Commander game modes. `game_type` is the client's `EGameModeId` and map names are its
//! `EMapId` strings (IEALobby.h, EALobby.cpp). Which maps a mode offers, its team and player counts
//! and the mode names are not in the client, so they are this server's choice.

use sc_proto::services::{
    custom_lobby::v1::{map_game_rules, MapGameRules},
    social::v1::{ClientGameModeSetting, ClientMapInfo},
};

const DOGFIGHT_MAPS: &[&str] = &["EA_BrokenMoon", "EA_DyingStar", "EA_Kareah", "EA_JerichoStation", "EA_RayariStation"];
const RACE_MAPS: &[&str] = &["EA_NHS_OldVanderval", "EA_NHS_DeffordLink", "EA_NHS_HalloranCircuit", "EA_NHS_Rikkord"];
const GRAV_RACE_MAPS: &[&str] = &["EA_DunlowDerby", "EA_CaplanCircuit", "EA_ClioIslands"];
const FPS_MAPS: &[&str] = &["EA_Demien", "EA_EZHab", "EA_Gundo", "EA_Echo11", "EA_MinersLament"];

pub struct Mode {
    pub name: &'static str,
    pub game_type: i32,
    pub teams: i32,
    pub min_players: i32,
    pub max_players: i32,
    pub maps: &'static [&'static str],
}

const fn mode(
    name: &'static str,
    game_type: i32,
    teams: i32,
    min_players: i32,
    max_players: i32,
    maps: &'static [&'static str],
) -> Mode {
    Mode { name, game_type, teams, min_players, max_players, maps }
}

pub const MODES: &[Mode] = &[
    mode("EA_FreeFlight", 1, 1, 1, 16, DOGFIGHT_MAPS),
    mode("EA_PirateSwarm", 2, 1, 1, 4, DOGFIGHT_MAPS),
    mode("EA_SquadronBattle", 3, 2, 2, 16, DOGFIGHT_MAPS),
    mode("EA_VanduulSwarm", 4, 1, 1, 4, DOGFIGHT_MAPS),
    mode("EA_ClassicRace", 5, 1, 1, 12, RACE_MAPS),
    mode("EA_Elimination", 6, 1, 2, 16, FPS_MAPS),
    mode("EA_TeamElimination", 7, 2, 2, 16, FPS_MAPS),
    mode("EA_Control", 8, 2, 2, 16, FPS_MAPS),
    mode("EA_Duel", 11, 2, 2, 2, DOGFIGHT_MAPS),
    mode("EA_GunGame", 12, 1, 2, 16, FPS_MAPS),
    mode("EA_GravRace", 27, 1, 1, 12, GRAV_RACE_MAPS),
];

const MAX_SQUAD_SIZE: i32 = 4;

pub fn client_settings() -> Vec<ClientGameModeSetting> {
    let map = |name: &&str| ClientMapInfo { map_name: name.to_string(), required_badge: 0, enabled: true };
    MODES
        .iter()
        .map(|mode| ClientGameModeSetting {
            game_mode: mode.name.to_string(),
            game_type: mode.game_type,
            required_badge: 0,
            enabled: true,
            maps: mode.maps.iter().map(map).collect(),
        })
        .collect()
}

/// The rules of a mode by name or by game type. An unknown mode gets the first one's.
pub fn find(name: &str) -> &'static Mode {
    let found = MODES.iter().find(|m| m.name.eq_ignore_ascii_case(name) || m.game_type.to_string() == name);
    found.unwrap_or(&MODES[0])
}

pub fn rules(mode: &Mode) -> MapGameRules {
    let map = |name: &&str| map_game_rules::Map { map_name: name.to_string(), max_player_count: mode.max_players };
    MapGameRules {
        maps: mode.maps.iter().map(map).collect(),
        team_count: mode.teams,
        min_player_count: mode.min_players,
        max_player_count: mode.max_players,
        max_squad_size: MAX_SQUAD_SIZE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_reaches_the_client_with_maps() {
        let settings = client_settings();
        assert_eq!(settings.len(), MODES.len());
        assert!(settings.iter().all(|mode| mode.enabled && !mode.maps.is_empty()));
    }

    #[test]
    fn modes_are_found_by_name_and_type() {
        assert_eq!(find("ea_duel").teams, 2);
        assert_eq!(find("27").name, "EA_GravRace");
        assert_eq!(find("unknown").name, MODES[0].name);
        assert_eq!(rules(find("EA_Duel")).team_count, 2);
    }
}
