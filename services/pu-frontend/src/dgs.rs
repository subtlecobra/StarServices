//! Which DGS governs a place, from the shard's `ShardTemplate`, and how to reach it.
//!
//! sc-shard-operator gives the replicant every DGS of the template in `SCBACKEND_DGS_AREAS`, and in
//! `SCBACKEND_DGS_ADDRESS` the address of a DGS's Service with `{name}` for its name. A container belongs to the
//! nearest root at or above it, which takes the tree to work out in general. A home location is simpler: it names its
//! planet, or has no planet above it and hangs from the system, where the `<system>system` root takes what no other
//! root claims. A location on a body that is not a root itself, such as a moon inside its planet's area, is not
//! resolved here, and has no such home location yet.

/// The game version in a live server name, which the shard id does not carry.
const GAME_VERSION: &str = "4102";

#[derive(Debug, PartialEq, Eq)]
struct Area {
    system: String,
    name: String,
    roots: Vec<String>,
    /// `None` with no address, on a shard from before DGSs could be reached.
    endpoint: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Directory {
    shard_id: String,
    areas: Vec<Area>,
}

/// One `<system>:<name>=<root>,<root>`.
fn area(entry: &str, address: &str) -> Result<Area, String> {
    let unreadable = || format!("unreadable DGS area '{entry}'");
    let (owner, roots) = entry.split_once('=').ok_or_else(unreadable)?;
    let (system, name) = owner.split_once(':').ok_or_else(unreadable)?;
    let endpoint = (!address.is_empty()).then(|| address.replace("{name}", name));
    Ok(Area { system: system.to_string(), name: name.to_string(), roots: roots.split(',').map(str::to_string).collect(), endpoint })
}

impl Directory {
    /// `areas` is space separated, as a root can hold no whitespace. It is empty for a shard with no template.
    pub fn parse(shard_id: &str, areas: &str, address: &str) -> Result<Self, String> {
        let areas = areas.split_whitespace().map(|entry| area(entry, address)).collect::<Result<_, _>>()?;
        Ok(Self { shard_id: shard_id.to_string(), areas })
    }

    pub fn dgs_count(&self) -> usize {
        self.areas.len()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.areas.iter().map(|area| area.name.as_str())
    }

    pub fn endpoint(&self, dgs: &str) -> Option<&str> {
        self.areas.iter().find(|area| area.name == dgs)?.endpoint.as_deref()
    }

    #[cfg(test)]
    pub fn set_endpoint(&mut self, dgs: &str, endpoint: String) {
        self.areas.iter_mut().find(|area| area.name == dgs).unwrap().endpoint = Some(endpoint);
    }

    /// The DGS of each system's entities: the one with its `<system>system` root.
    pub fn system_owners(&self) -> Vec<&str> {
        let owns = |area: &&Area| area.roots.contains(&format!("{}system", area.system));
        self.areas.iter().filter(owns).map(|area| area.name.as_str()).collect()
    }

    /// `system` is a location's, such as `Pyro`, and `planet` is the `name` of its planet's container.
    pub fn governing(&self, system: &str, planet: Option<&str>) -> Option<&str> {
        let system = system.to_ascii_lowercase();
        let root = planet.map_or_else(|| format!("{system}system"), str::to_string);
        let area = self.areas.iter().find(|area| area.system == system && area.roots.contains(&root))?;
        Some(&area.name)
    }

    /// The name the overlay shows for a DGS. The live form is `pub-use1b-sc-alpha-4102-12974691-game-92`, and this
    /// shard's is `pub-usa-sc-4102-12913470-dgs-pyro-1`.
    pub fn server_name(&self, dgs: &str) -> String {
        let fields: Vec<&str> = self.shard_id.rsplitn(3, '_').collect();
        match fields.as_slice() {
            [_slot, build, cluster] => format!("{}-sc-{GAME_VERSION}-{build}-dgs-{dgs}", cluster.replace('_', "-")),
            _ => format!("{}-dgs-{dgs}", self.shard_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_dgs::location::home_location;

    /// The roots of the home locations in the `default` template of the chart.
    const AREAS: &str = "stanton:stanton-1=stanton1 stanton:stanton-3=stanton2,stanton2_l1 stanton:stanton-5=stanton4 \
        stanton:stanton-7=stanton3,stantonsystem pyro:pyro-1=pyro1,pyro2,pyro3,pyrosystem pyro:pyro-2=pyro5,pyro6 \
        nyx:nyx-1=nyxsystem";

    fn directory() -> Directory {
        Directory::parse("pub_usa_12913470_010", AREAS, "http://dgs-{name}.ns.svc:50051").unwrap()
    }

    #[test]
    fn a_planet_goes_to_the_dgs_with_its_root() {
        assert_eq!(directory().governing("Pyro", Some("pyro6")), Some("pyro-2"));
        assert_eq!(directory().governing("Stanton", Some("stanton1")), Some("stanton-1"));
    }

    #[test]
    fn a_place_under_no_planet_goes_to_the_root_of_its_system() {
        assert_eq!(directory().governing("Pyro", None), Some("pyro-1"));
        assert_eq!(directory().governing("Nyx", None), Some("nyx-1"));
    }

    #[test]
    fn a_body_no_root_names_has_no_dgs_rather_than_a_wrong_one() {
        assert_eq!(directory().governing("Stanton", Some("stanton2a")), None);
    }

    #[test]
    fn a_root_only_counts_in_its_own_system() {
        assert_eq!(directory().governing("Nyx", Some("pyro6")), None);
    }

    #[test]
    fn every_home_location_has_a_dgs() {
        let ids = [1105811199, 2026442305, 2273540638, 3170699229, 3723364946, 4005457614, 810966700, 848868938];
        for id in ids {
            let location = home_location(id).unwrap();
            let planet = location.planet.as_ref().map(|planet| planet.name.as_str());
            assert!(directory().governing(&location.system, planet).is_some(), "{}", location.name);
        }
    }

    #[test]
    fn the_name_follows_the_live_form() {
        assert_eq!(directory().server_name("pyro-1"), "pub-usa-sc-4102-12913470-dgs-pyro-1");
    }

    #[test]
    fn a_dgs_is_reached_at_its_own_name() {
        assert_eq!(directory().endpoint("pyro-2"), Some("http://dgs-pyro-2.ns.svc:50051"));
        assert_eq!(directory().endpoint("pyro-9"), None);
    }

    #[test]
    fn no_address_reaches_no_dgs_but_still_says_who_governs() {
        let directory = Directory::parse("local", "pyro:pyro-1=pyrosystem", "").unwrap();
        assert_eq!(directory.endpoint("pyro-1"), None);
        assert_eq!(directory.governing("Pyro", None), Some("pyro-1"));
    }

    #[test]
    fn a_system_belongs_to_the_dgs_with_its_system_root() {
        assert_eq!(directory().system_owners(), ["stanton-7", "pyro-1", "nyx-1"]);
    }

    #[test]
    fn a_shard_id_of_another_form_is_used_whole() {
        let directory = Directory::parse("local", "pyro:pyro-1=pyro3", "").unwrap();
        assert_eq!(directory.server_name("pyro-1"), "local-dgs-pyro-1");
    }

    #[test]
    fn no_areas_means_no_dgs() {
        let directory = Directory::parse("local", "", "").unwrap();
        assert_eq!((directory.dgs_count(), directory.governing("Pyro", None)), (0, None));
    }

    #[test]
    fn an_unreadable_area_is_refused() {
        assert!(Directory::parse("local", "pyro-1", "").is_err());
        assert!(Directory::parse("local", "pyro-1=pyro3", "").is_err());
    }
}
