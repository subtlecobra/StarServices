//! The scaling decisions, kept free of Kubernetes so they can be tested directly.

use crate::crd::ShardClusterSpec;

const SHARD_ID_STEP: u32 = 10;
const NAMESPACE_PREFIX: &str = "scbackend-shard-";
const MAX_DNS_LABEL: usize = 63;
const HASH_LEN: usize = 8;

const ADJECTIVES: [&str; 32] = [
    "amber", "azure", "bold", "brave", "bright", "cobalt", "crimson", "dark", "distant", "dusty", "ember",
    "fallen", "frozen", "gilded", "golden", "hidden", "iron", "jade", "lone", "lunar", "molten", "noble",
    "pale", "quiet", "rapid", "rogue", "scarlet", "silent", "silver", "solar", "swift", "wild",
];
const NOUNS: [&str; 32] = [
    "anvil", "arrow", "aurora", "beacon", "comet", "corsair", "drifter", "eclipse", "falcon", "forge",
    "frontier", "harbor", "hawk", "horizon", "hunter", "lantern", "mantis", "meridian", "nebula", "nomad",
    "orbit", "outpost", "phoenix", "pilgrim", "pulsar", "raven", "sentinel", "serpent", "sparrow", "vanguard",
    "voyager", "wayfarer",
];

/// A random name for a shard's pod and service, such as `usa-crimson-falcon`. Each value of `seed`
/// picks one pair, and a clash with a running shard only delays that shard until the next pass.
pub fn shard_name(cluster: &str, seed: u64) -> String {
    let adjective = ADJECTIVES[(seed % ADJECTIVES.len() as u64) as usize];
    let noun = NOUNS[((seed / ADJECTIVES.len() as u64) % NOUNS.len() as u64) as usize];
    format!("{cluster}-{adjective}-{noun}")
}

/// The name the game and the status show. Kubernetes object names cannot hold underscores, so only
/// the pod and Service keep the dashes of `shard_name`.
pub fn display_name(name: &str) -> String {
    name.replace('-', "_")
}

/// The last part of a shard id, which goes up in tens from 10.
fn short_id(slot: u32) -> String {
    format!("{:03}", (slot + 1) * SHARD_ID_STEP)
}

/// The id the game sees, in the format of real shards such as `pub_euw1b_12881860_060`.
pub fn shard_id(cluster: &str, build: &str, slot: u32) -> String {
    format!("pub_{cluster}_{build}_{}", short_id(slot))
}

/// FNV-1a, because a namespace name must not change with the Rust release the operator is built with.
fn stable_hash(text: &str) -> u32 {
    text.bytes().fold(0x811c_9dc5, |hash, byte| (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193))
}

/// `scbackend-shard-<region>-<short id>`. A region that does not fit a DNS label as it is gets cut or cleaned, and
/// a hash of the original keeps it apart from other regions that end up alike.
pub fn shard_namespace(region: &str, slot: u32) -> String {
    let id = short_id(slot);
    let room = MAX_DNS_LABEL - NAMESPACE_PREFIX.len() - id.len() - 1;
    let clean: String = region.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let clean = clean.trim_matches('-');
    if clean == region && clean.len() <= room {
        return format!("{NAMESPACE_PREFIX}{clean}-{id}");
    }
    let cut = clean[..clean.len().min(room - HASH_LEN - 1)].trim_end_matches('-');
    let hash = stable_hash(region);
    match cut.is_empty() {
        true => format!("{NAMESPACE_PREFIX}{hash:08x}-{id}"),
        false => format!("{NAMESPACE_PREFIX}{cut}-{hash:08x}-{id}"),
    }
}

/// Enough shards for the demand, plus the warm ones, up to the limit.
pub fn desired_shards(spec: &ShardClusterSpec, demand: u32) -> u32 {
    let needed = demand.div_ceil(spec.players_per_shard.max(1));
    (needed + spec.warm_shards).min(spec.max_shards)
}

/// The lowest slots not in use. A slot is in use as long as anything of a shard before it remains, which keeps its
/// namespace name and NodePort from being taken too soon.
pub fn free_slots(used: &[u32], count: u32, max_shards: u32) -> Vec<u32> {
    (0..max_shards).filter(|slot| !used.contains(slot)).take(count as usize).collect()
}

/// How many reservations to drop now that a shard reports more players: those players have arrived,
/// and counting them twice would open shards nobody needs.
pub fn arrived(previous_players: u32, players: u32) -> usize {
    players.saturating_sub(previous_players) as usize
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub slot: u32,
    pub ready: bool,
    pub players: u32,
    pub reserved: u32,
}

/// Fills the busiest shard that still has room, which leaves the others empty and able to close.
pub fn pick_shard(candidates: &[Candidate], capacity: u32) -> Option<u32> {
    candidates
        .iter()
        .filter(|c| c.ready && c.players + c.reserved < capacity)
        .max_by_key(|c| (c.players + c.reserved, std::cmp::Reverse(c.slot)))
        .map(|c| c.slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(warm: u32, max: u32, per_shard: u32) -> ShardClusterSpec {
        ShardClusterSpec {
            region: "usa".into(),
            image: "x".into(),
            build: "12913470".into(),
            public_address: "127.0.0.2".into(),
            public_port_base: 64090,
            node_port_base: 30090,
            warm_shards: warm,
            max_shards: max,
            players_per_shard: per_shard,
            scale_down_delay_seconds: 300,
            shard_template: "default".into(),
        }
    }

    #[test]
    fn shard_ids_follow_the_real_format() {
        assert_eq!(shard_id("usa", "12913470", 0), "pub_usa_12913470_010");
        assert_eq!(shard_id("usa", "12913470", 7), "pub_usa_12913470_080");
    }

    fn valid_dns_label(name: &str) -> bool {
        let inner = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
        name.len() <= 63 && name.chars().all(inner) && !name.starts_with('-') && !name.ends_with('-')
    }

    #[test]
    fn a_shard_namespace_has_the_region_and_short_id() {
        assert_eq!(shard_namespace("usa", 0), "scbackend-shard-usa-010");
        assert_eq!(shard_namespace("euw1b", 9), "scbackend-shard-euw1b-100");
    }

    #[test]
    fn a_region_that_just_fits_is_kept_whole() {
        let region = "a".repeat(43);
        assert_eq!(shard_namespace(&region, 0), format!("scbackend-shard-{region}-010"));
        assert_eq!(shard_namespace(&region, 0).len(), 63);
    }

    #[test]
    fn shard_namespaces_are_dns_labels_for_any_region() {
        let long = "a-very-long-region-name-".repeat(5);
        for region in ["usa", "EU_West", "eu.west-1", "-usa-", "", "___", "\u{e9}t\u{e9}", long.as_str()] {
            for slot in [0, 9, 99, 9999] {
                let name = shard_namespace(region, slot);
                assert!(valid_dns_label(&name), "{name}");
                assert!(name.starts_with("scbackend-shard-"), "{name}");
                assert!(name.ends_with(&format!("-{}", short_id(slot))), "{name}");
            }
        }
    }

    #[test]
    fn regions_that_clean_up_alike_keep_separate_namespaces() {
        assert_ne!(shard_namespace("eu_west", 0), shard_namespace("eu-west", 0));
        assert_ne!(shard_namespace("EU-WEST", 0), shard_namespace("eu-west", 0));
        let long = "x".repeat(70);
        assert_ne!(shard_namespace(&format!("{long}1"), 0), shard_namespace(&format!("{long}2"), 0));
    }

    #[test]
    fn a_namespace_name_never_changes_between_builds() {
        assert_eq!(shard_namespace("EU_West", 0), "scbackend-shard-eu-west-723f4487-010");
    }

    #[test]
    fn shard_names_are_valid_kubernetes_names() {
        for seed in [0, 1, 31, 32, 1023, u64::MAX] {
            let name = shard_name("usa", seed);
            assert!(name.starts_with("usa-") && name.len() < 63, "{name}");
            assert!(name.chars().all(|c| c.is_ascii_lowercase() || c == '-'), "{name}");
        }
        assert_ne!(shard_name("usa", 0), shard_name("usa", 1));
    }

    #[test]
    fn display_names_use_underscores() {
        assert_eq!(display_name("usa-crimson-falcon"), "usa_crimson_falcon");
    }

    #[test]
    fn keeps_one_warm_shard_with_no_players() {
        assert_eq!(desired_shards(&spec(1, 10, 50), 0), 1);
    }

    #[test]
    fn stays_one_ahead_of_demand() {
        let spec = spec(1, 10, 50);
        assert_eq!(desired_shards(&spec, 1), 2);
        assert_eq!(desired_shards(&spec, 50), 2);
        assert_eq!(desired_shards(&spec, 51), 3);
    }

    #[test]
    fn never_exceeds_the_limit() {
        assert_eq!(desired_shards(&spec(1, 3, 50), 10_000), 3);
    }

    #[test]
    fn only_new_players_consume_reservations() {
        assert_eq!(arrived(0, 2), 2);
        assert_eq!(arrived(2, 2), 0);
        assert_eq!(arrived(3, 1), 0);
    }

    #[test]
    fn reuses_the_lowest_free_slots() {
        assert_eq!(free_slots(&[0, 2], 2, 10), [1, 3]);
        assert_eq!(free_slots(&[0, 1], 5, 3), [2]);
    }

    #[test]
    fn a_slot_held_by_a_closing_shard_is_passed_over_and_the_next_one_opens() {
        assert_eq!(free_slots(&[0], 1, 10), [1]);
        assert_eq!(free_slots(&[0, 1, 2], 1, 3), [] as [u32; 0]);
    }

    #[test]
    fn packs_players_and_skips_full_or_unready_shards() {
        let shard = |slot, ready, players, reserved| Candidate { slot, ready, players, reserved };
        let shards = [shard(0, true, 2, 0), shard(1, true, 5, 0), shard(2, false, 9, 0), shard(3, true, 10, 0)];
        assert_eq!(pick_shard(&shards, 10), Some(1));
        assert_eq!(pick_shard(&[shard(0, true, 9, 1)], 10), None);
    }
}
