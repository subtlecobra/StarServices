//! URNs as the client writes them (`SUrn::ToString`, 0x1404329f0): `urn:sc:<namespace>:<kind>:<id type>:<id>`.

const PLAYER_PREFIX: &str = "urn:sc:global:player:geid:";
const GROUP_PREFIX: &str = "urn:sc:global:group:uuid:";

pub fn player(geid: u64) -> String {
    format!("{PLAYER_PREFIX}{geid}")
}

/// The GEID of a player URN. The client compares the prefix without regard to case.
pub fn player_geid(urn: &str) -> Option<u64> {
    let prefix = urn.get(..PLAYER_PREFIX.len())?;
    prefix.eq_ignore_ascii_case(PLAYER_PREFIX).then(|| urn[PLAYER_PREFIX.len()..].parse().ok())?
}

pub fn group(id: &str) -> String {
    format!("{GROUP_PREFIX}{id}")
}

/// The id in a group URN.
pub fn group_id(urn: &str) -> Option<&str> {
    urn.strip_prefix(GROUP_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_urns_round_trip() {
        assert_eq!(player(300000000001), "urn:sc:global:player:geid:300000000001");
        assert_eq!(player_geid(&player(300000000001)), Some(300000000001));
        assert_eq!(player_geid("URN:SC:GLOBAL:PLAYER:GEID:7"), Some(7));
    }

    #[test]
    fn group_urns_round_trip() {
        assert_eq!(group_id(&group("abc")), Some("abc"));
        assert_eq!(group_id(&player(7)), None);
    }

    #[test]
    fn other_urns_are_not_players() {
        assert_eq!(player_geid("urn:sc:global:account:integer:7"), None);
        assert_eq!(player_geid("urn:sc:global:player:geid:x"), None);
        assert_eq!(player_geid(""), None);
    }
}
