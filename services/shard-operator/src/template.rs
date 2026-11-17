//! Checks a ShardTemplate and turns it into what each DGS is told, kept free of Kubernetes like scaling.rs.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;

use crate::crd::ShardTemplateSpec;

const KNOWN_SYSTEMS: [&str; 3] = ["stanton", "pyro", "nyx"];
/// Leaves a pod name, with the shard name in front, inside the 63 characters of a label value and hostname.
const MAX_DGS_NAME: usize = 24;

/// An entry of `shardTemplates` in the chart values.
#[derive(Clone, Debug, Deserialize)]
pub struct ChartTemplate {
    pub name: String,
    pub spec: ShardTemplateSpec,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DgsAssignment {
    pub name: String,
    pub system: String,
    pub roots: Vec<String>,
    /// The roots of the other DGSs of the system, which this one must not enter.
    pub excluded: Vec<String>,
}

impl DgsAssignment {
    /// Compared with the running pod, to tell when its area has changed.
    pub fn area(&self) -> String {
        format!("{}:{}:{}", self.system, self.roots.join(","), self.excluded.join(","))
    }
}

fn valid_dgs_name(name: &str) -> bool {
    let inner = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
    !name.is_empty()
        && name.len() <= MAX_DGS_NAME
        && name.chars().all(inner)
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// Roots reach the DGS as a comma separated list.
fn valid_root(root: &str) -> bool {
    !root.is_empty() && !root.contains(',') && !root.contains(char::is_whitespace)
}

/// Whether the roots cover the whole tree can only be told from the tree. A DGS names a root the tree lacks in
/// its log and stays unready.
pub fn validate(spec: &ShardTemplateSpec) -> Result<(), String> {
    if spec.systems.is_empty() {
        return Err("no systems".to_string());
    }
    let mut systems = HashSet::new();
    let mut names = HashSet::new();
    for system in &spec.systems {
        if !KNOWN_SYSTEMS.contains(&system.name.as_str()) {
            return Err(format!("unknown system '{}'", system.name));
        }
        if !systems.insert(&system.name) {
            return Err(format!("system '{}' listed twice", system.name));
        }
        if system.dgs.is_empty() {
            return Err(format!("system '{}' has no DGS", system.name));
        }
        let mut owners: HashMap<&str, &str> = HashMap::new();
        for dgs in &system.dgs {
            if !valid_dgs_name(&dgs.name) {
                return Err(format!("'{}' is not a valid DGS name", dgs.name));
            }
            if !names.insert(&dgs.name) {
                return Err(format!("DGS '{}' listed twice", dgs.name));
            }
            if dgs.roots.is_empty() {
                return Err(format!("DGS '{}' has no roots", dgs.name));
            }
            for root in &dgs.roots {
                if !valid_root(root) {
                    return Err(format!("DGS '{}': '{root}' is not a valid root", dgs.name));
                }
                if let Some(owner) = owners.insert(root, &dgs.name) {
                    return Err(format!("root '{root}' of {} is in both '{owner}' and '{}'", system.name, dgs.name));
                }
            }
        }
    }
    Ok(())
}

/// The DGSs of a valid template, in its order.
pub fn assignments(spec: &ShardTemplateSpec) -> Result<Vec<DgsAssignment>, String> {
    validate(spec)?;
    let mut all = Vec::new();
    for system in &spec.systems {
        for dgs in &system.dgs {
            let others = system.dgs.iter().filter(|other| other.name != dgs.name);
            all.push(DgsAssignment {
                name: dgs.name.clone(),
                system: system.name.clone(),
                roots: dgs.roots.clone(),
                excluded: others.flat_map(|other| other.roots.iter().cloned()).collect(),
            });
        }
    }
    Ok(all)
}

/// Tells a replicant which DGS governs what, as `<system>:<name>=<root>,<root>`. Whitespace separates them, as
/// `valid_root` keeps it out of a root.
pub fn dgs_areas(dgs: &[DgsAssignment]) -> String {
    let area = |dgs: &DgsAssignment| format!("{}:{}={}", dgs.system, dgs.name, dgs.roots.join(","));
    dgs.iter().map(area).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{DgsLayout, SystemLayout};

    fn dgs(name: &str, roots: &[&str]) -> DgsLayout {
        DgsLayout { name: name.into(), roots: roots.iter().map(ToString::to_string).collect() }
    }

    fn template(systems: Vec<(&str, Vec<DgsLayout>)>) -> ShardTemplateSpec {
        let systems = systems.into_iter().map(|(name, dgs)| SystemLayout { name: name.into(), dgs });
        ShardTemplateSpec { systems: systems.collect() }
    }

    fn error(spec: &ShardTemplateSpec) -> String {
        validate(spec).unwrap_err()
    }

    #[test]
    fn a_replicant_is_told_every_dgs_with_its_system_and_roots() {
        let spec = template(vec![("pyro", vec![dgs("pyro-1", &["pyro1", "pyro3"]), dgs("pyro-2", &["pyro6"])]), ("nyx", vec![dgs("nyx-1", &["nyxsystem"])])]);
        let areas = dgs_areas(&assignments(&spec).unwrap());
        assert_eq!(areas, "pyro:pyro-1=pyro1,pyro3 pyro:pyro-2=pyro6 nyx:nyx-1=nyxsystem");
    }

    #[test]
    fn a_root_belongs_to_one_dgs() {
        let spec = template(vec![("stanton", vec![dgs("a", &["stanton1"]), dgs("b", &["stanton2", "stanton1"])])]);
        assert!(error(&spec).contains("in both 'a' and 'b'"), "{}", error(&spec));
        assert!(validate(&template(vec![("stanton", vec![dgs("a", &["x", "x"])])])).is_err());
    }

    #[test]
    fn systems_scope_root_names() {
        let spec = template(vec![("stanton", vec![dgs("a", &["hub"])]), ("pyro", vec![dgs("b", &["hub"])])]);
        assert_eq!(validate(&spec), Ok(()));
    }

    #[test]
    fn only_known_systems() {
        let spec = template(vec![("terra", vec![dgs("a", &["terra1"])])]);
        assert_eq!(error(&spec), "unknown system 'terra'");
        assert!(validate(&template(vec![("Stanton", vec![dgs("a", &["x"])])])).is_err());
    }

    #[test]
    fn a_system_appears_once_and_a_template_is_not_empty() {
        let spec = template(vec![("nyx", vec![dgs("a", &["x"])]), ("nyx", vec![dgs("b", &["y"])])]);
        assert_eq!(error(&spec), "system 'nyx' listed twice");
        assert_eq!(error(&template(vec![])), "no systems");
        assert_eq!(error(&template(vec![("nyx", vec![])])), "system 'nyx' has no DGS");
    }

    #[test]
    fn dgs_names_are_unique_across_systems() {
        let spec = template(vec![("stanton", vec![dgs("a", &["x"])]), ("pyro", vec![dgs("a", &["y"])])]);
        assert_eq!(error(&spec), "DGS 'a' listed twice");
    }

    #[test]
    fn dgs_names_fit_in_a_pod_name() {
        for name in ["", "-a", "a-", "Stanton-1", "a_b", "a.b", &"a".repeat(25)] {
            let spec = template(vec![("stanton", vec![dgs(name, &["x"])])]);
            assert!(validate(&spec).is_err(), "{name}");
        }
    }

    #[test]
    fn roots_survive_the_comma_separated_list() {
        for root in ["", "a,b", "a b"] {
            assert!(validate(&template(vec![("stanton", vec![dgs("a", &[root])])])).is_err(), "{root}");
        }
        assert_eq!(error(&template(vec![("stanton", vec![dgs("a", &[])])])), "DGS 'a' has no roots");
    }

    #[test]
    fn each_dgs_excludes_the_other_roots_of_its_system_only() {
        let spec = template(vec![
            ("stanton", vec![dgs("a", &["stanton1"]), dgs("b", &["stanton1a", "stanton1c"])]),
            ("nyx", vec![dgs("c", &["nyxsystem"])]),
        ]);
        let all = assignments(&spec).unwrap();
        assert_eq!(all[0].excluded, ["stanton1a", "stanton1c"]);
        assert_eq!(all[1].excluded, ["stanton1"]);
        assert!(all[2].excluded.is_empty());
        assert_eq!(all[0].area(), "stanton:stanton1:stanton1a,stanton1c");
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ChartValues {
        shard_templates: Vec<ChartTemplate>,
    }

    #[test]
    fn the_default_template_is_the_layout_of_docs_dgs_zones() {
        let values = include_str!("../../../deploy/charts/sc-shard-operator/values.yaml");
        let values: ChartValues = serde_yaml::from_str(values).unwrap();
        let default = values.shard_templates.iter().find(|template| template.name == "default").unwrap();
        let all = assignments(&default.spec).unwrap();
        let count = |system: &str| all.iter().filter(|dgs| dgs.system == system).count();
        assert_eq!((count("stanton"), count("pyro"), count("nyx")), (7, 2, 1));
    }
}
