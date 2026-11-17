//! What the inventory page offers to add: the item categories of the game's own inventory filters
//! (`PlayerChoice_PITConfig`, `inventoryParams.filters` in the DataCore) with their sub-filters, each a set of
//! (item type, item sub type) pairs, plus the armor weights, which the game does not filter by. A group's choices are
//! OR, the groups are AND. The page's script reads the table from the page; the search turns the chosen choices into
//! the pairs the classes must match, and `Other` is every class no category holds.

use serde_json::{json, Value};

/// An item type, and one sub type of it; `""` is any.
pub type Pair = (&'static str, &'static str);

/// How a group's choices select classes: by (type, sub type) pairs, or by sub type alone (an armor's weight).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum By {
    Pairs,
    Sub,
}

pub struct Choice {
    pub id: &'static str,
    pub label: &'static str,
    pub pairs: &'static [Pair],
    pub subs: &'static [&'static str],
}

pub struct Group {
    pub id: &'static str,
    pub label: &'static str,
    pub by: By,
    pub choices: &'static [Choice],
}

pub struct Category {
    pub id: &'static str,
    pub label: &'static str,
    /// The game's own sub-filters: together the category's classes. `Other` has none and is what the rest leave.
    pub groups: &'static [Group],
}

const fn kind(id: &'static str, label: &'static str, pairs: &'static [Pair]) -> Choice {
    Choice { id, label, pairs, subs: &[] }
}

const fn weight(id: &'static str, label: &'static str, subs: &'static [&'static str]) -> Choice {
    Choice { id, label, pairs: &[], subs }
}

pub const OTHER: &str = "other";

pub const CATEGORIES: &[Category] = &[
    Category {
        id: "weapons",
        label: "Weapons",
        groups: &[Group {
            id: "kind",
            label: "Kind",
            by: By::Pairs,
            choices: &[
                kind("sidearms", "Sidearms", &[("WeaponPersonal", "Small")]),
                kind("primaries", "Primaries", &[("WeaponPersonal", "Medium")]),
                kind("specials", "Specials", &[("WeaponPersonal", "Large")]),
                kind("melee", "Melee", &[("WeaponPersonal", "Knife")]),
                kind("throwables", "Throwables", &[("WeaponPersonal", "Grenade")]),
                kind("attachments", "Attachments", &[("WeaponAttachment", "IronSight"), ("WeaponAttachment", "BottomAttachment"), ("WeaponAttachment", "Barrel")]),
            ],
        }],
    },
    Category {
        id: "armor",
        label: "Armor",
        groups: &[
            Group {
                id: "part",
                label: "Part",
                by: By::Pairs,
                choices: &[
                    kind("undersuits", "Undersuits", &[("Char_Armor_Undersuit", "")]),
                    kind("helmets", "Helmets", &[("Char_Armor_Helmet", "")]),
                    kind("core", "Core", &[("Char_Armor_Torso", "")]),
                    kind("arms", "Arms", &[("Char_Armor_Arms", "")]),
                    kind("legs", "Legs", &[("Char_Armor_Legs", "")]),
                    kind("backpacks", "Backpacks", &[("Char_Armor_Backpack", "")]),
                    kind("flair", "Flair", &[("Char_Flair", "")]),
                ],
            },
            Group {
                id: "weight",
                label: "Weight",
                by: By::Sub,
                choices: &[weight("light", "Light", &["Light", "LightArmor"]), weight("medium", "Medium", &["Medium"]), weight("heavy", "Heavy", &["Heavy"])],
            },
        ],
    },
    Category {
        id: "clothing",
        label: "Clothing",
        groups: &[Group {
            id: "part",
            label: "Part",
            by: By::Pairs,
            choices: &[
                kind("hats", "Hats", &[("Char_Clothing_Hat", "")]),
                kind("eyewear", "Eyewear", &[("Char_Accessory_Eyes", "")]),
                kind("torso0", "Shirts", &[("Char_Clothing_Torso_0", "")]),
                kind("torso1", "Jackets", &[("Char_Clothing_Torso_1", ""), ("Char_Clothing_Torso_2", "")]),
                kind("hands", "Hands", &[("Char_Clothing_Hands", "")]),
                kind("legs", "Legs", &[("Char_Clothing_Legs", "")]),
                kind("feet", "Feet", &[("Char_Clothing_Feet", "")]),
            ],
        }],
    },
    Category {
        id: "utility",
        label: "Utility",
        groups: &[Group {
            id: "kind",
            label: "Kind",
            by: By::Pairs,
            choices: &[
                kind("gadgets", "Gadgets", &[("WeaponPersonal", "Gadget"), ("Gadget", "Gadget")]),
                kind("gadgetattachments", "Gadget attachments", &[("WeaponAttachment", "Utility")]),
                kind("medical", "Medical", &[("FPS_Consumable", "MedPack"), ("FPS_Consumable", "OxygenCap")]),
                kind("chips", "Hacking chips", &[("FPS_Consumable", "Hacking")]),
                kind("technology", "Technology", &[("MobiGlas", "Personal")]),
            ],
        }],
    },
    Category {
        id: "ammo",
        label: "Ammo",
        groups: &[Group { id: "kind", label: "Kind", by: By::Pairs, choices: &[kind("magazines", "Magazines", &[("WeaponAttachment", "Magazine")])] }],
    },
    Category {
        id: "vehicle",
        label: "Vehicle parts",
        groups: &[Group {
            id: "part",
            label: "Part",
            by: By::Pairs,
            choices: &[
                kind("ordnance", "Ordnance", &[("Bomb", ""), ("Missile", "")]),
                kind("missilerack", "Missile racks", &[("MissileLauncher", "")]),
                kind("weapon", "Weapons", &[("WeaponGun", "")]),
                kind("turret", "Turrets", &[("Turret", "")]),
                kind("countermeasure", "Countermeasures", &[("WeaponDefensive", "")]),
                kind("powerplant", "Power plants", &[("PowerPlant", "")]),
                kind("shield", "Shields", &[("Shield", "")]),
                kind("cooler", "Coolers", &[("Cooler", "")]),
                kind("quantumdrive", "Quantum drives", &[("QuantumDrive", "")]),
                kind("jumpdrive", "Jump drives", &[("JumpDrive", "")]),
                kind("radar", "Radar", &[("Radar", "")]),
                kind("emp", "EMP", &[("EMP", "")]),
                kind("mining", "Mining", &[("MiningModifier", ""), ("WeaponMining", "Gun")]),
                kind("paint", "Paints", &[("Paints", "")]),
            ],
        }],
    },
    Category {
        id: "sustenance",
        label: "Sustenance",
        groups: &[Group { id: "kind", label: "Kind", by: By::Pairs, choices: &[kind("food", "Food", &[("Food", "")]), kind("drink", "Drinks", &[("Drink", "")])] }],
    },
    Category {
        id: "containers",
        label: "Containers",
        groups: &[Group {
            id: "kind",
            label: "Kind",
            by: By::Pairs,
            choices: &[kind("cargo", "Cargo", &[("Cargo", "")]), kind("lootable", "Lootable containers", &[("Container", ""), ("InventoryContainer", "")])],
        }],
    },
    Category { id: OTHER, label: "Other", groups: &[] },
];

/// How many of an item one stack holds, 0 for an item that does not stack. A class stacks when the game says so: its
/// record carries the tag `Item.Inventory.Stackable` (`stackable`, from the seed). The limit is the game's
/// (`PersonalThoughtInventoryParams`): 99, and 999 for currency.
pub fn stack_limit(item_type: &str, stackable: bool) -> u32 {
    match item_type {
        "Currency" => 999,
        _ if stackable => 99,
        _ => 0,
    }
}

/// What a search takes: the (type, sub types) any class must match one of (a type with no sub types takes any), those it
/// must match none of, and the sub types it must have.
#[derive(Debug, Default, PartialEq)]
pub struct Selection {
    pub any_of: Vec<(String, Vec<String>)>,
    pub none_of: Vec<(String, Vec<String>)>,
    pub sub_types: Vec<String>,
}

fn group_pairs(pairs: impl Iterator<Item = Pair>) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    let mut open: Vec<bool> = Vec::new();
    for (item_type, sub) in pairs {
        let at = match out.iter().position(|(known, _)| known == item_type) {
            Some(at) => at,
            None => {
                out.push((item_type.to_string(), Vec::new()));
                open.push(false);
                out.len() - 1
            }
        };
        if sub.is_empty() {
            open[at] = true;
        } else if !out[at].1.iter().any(|known| known == sub) {
            out[at].1.push(sub.to_string());
        }
    }
    for (at, whole) in open.into_iter().enumerate() {
        if whole {
            out[at].1.clear();
        }
    }
    out
}

fn all_pairs(category: &Category) -> impl Iterator<Item = Pair> + '_ {
    category.groups.iter().filter(|group| group.by == By::Pairs).flat_map(|group| group.choices.iter().flat_map(|choice| choice.pairs.iter().copied()))
}

/// The search for a category and the chosen choice ids (of any of its groups). A group with none chosen does not narrow
/// it. `Other` is every class no category holds. `None` for an unknown category.
pub fn selection(category: &str, chosen: &[&str]) -> Option<Selection> {
    let category = CATEGORIES.iter().find(|candidate| candidate.id == category)?;
    if category.id == OTHER {
        return Some(Selection { none_of: group_pairs(CATEGORIES.iter().flat_map(all_pairs)), ..Default::default() });
    }
    let mut picked: Vec<Pair> = Vec::new();
    let mut sub_types: Vec<String> = Vec::new();
    for group in category.groups {
        for choice in group.choices.iter().filter(|choice| chosen.contains(&choice.id)) {
            match group.by {
                By::Pairs => picked.extend(choice.pairs.iter().copied()),
                By::Sub => sub_types.extend(choice.subs.iter().map(|name| name.to_string())),
            }
        }
    }
    let any_of = if picked.is_empty() { group_pairs(all_pairs(category)) } else { group_pairs(picked.into_iter()) };
    Some(Selection { any_of, none_of: Vec::new(), sub_types })
}

/// The table as the page carries it for the script: each category's groups and choices by id, the item types of each
/// category, the label of each (type, sub type) a choice holds, and the weight each armor sub type stands for.
pub fn table() -> Value {
    let mut labels = serde_json::Map::new();
    let mut weights = serde_json::Map::new();
    for category in CATEGORIES {
        for group in category.groups {
            for choice in group.choices {
                for (item_type, sub) in choice.pairs {
                    labels.insert(if sub.is_empty() { item_type.to_string() } else { format!("{item_type}:{sub}") }, json!(choice.label));
                }
                if group.id == "weight" {
                    for sub in choice.subs {
                        weights.insert((*sub).to_string(), json!({ "id": choice.id, "label": choice.label }));
                    }
                }
            }
        }
    }
    json!({
        "categories": CATEGORIES.iter().map(|category| {
            let mut types: Vec<&str> = all_pairs(category).map(|(item_type, _)| item_type).collect();
            types.dedup();
            json!({
                "id": category.id,
                "label": category.label,
                "types": types,
                "groups": category.groups.iter().map(|group| json!({
                    "id": group.id,
                    "label": group.label,
                    "choices": group.choices.iter().map(|choice| json!({ "id": choice.id, "label": choice.label })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
        "labels": labels,
        "weights": weights,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_and_no_pair_is_in_two_choices() {
        let mut pairs = HashSet::new();
        for category in CATEGORIES {
            let mut ids = HashSet::new();
            for group in category.groups {
                assert!(!group.choices.is_empty(), "{} {}", category.id, group.id);
                for choice in group.choices {
                    assert!(ids.insert(choice.id), "{} {}", category.id, choice.id);
                    for pair in choice.pairs {
                        assert!(pairs.insert(*pair), "{pair:?} twice");
                    }
                }
            }
        }
    }

    #[test]
    fn a_selection_is_the_chosen_parts_and_weights_or_the_whole_category() {
        let picked = selection("armor", &["core", "arms", "heavy"]).unwrap();
        assert_eq!(picked.any_of, [("Char_Armor_Torso".to_string(), vec![]), ("Char_Armor_Arms".to_string(), vec![])]);
        assert_eq!(picked.sub_types, ["Heavy"]);
        let light = selection("armor", &["light"]).unwrap();
        assert_eq!(light.any_of.len(), 7, "no part chosen: every armor part");
        assert_eq!(light.sub_types, ["Light", "LightArmor"]);
        let weapons = selection("weapons", &["sidearms", "melee"]).unwrap();
        assert_eq!(weapons.any_of, [("WeaponPersonal".to_string(), vec!["Small".to_string(), "Knife".to_string()])]);
        let attachments = selection("weapons", &["attachments"]).unwrap();
        assert_eq!(attachments.any_of, [("WeaponAttachment".to_string(), vec!["IronSight".to_string(), "BottomAttachment".to_string(), "Barrel".to_string()])]);
        assert!(selection("nonsense", &[]).is_none());
        // A choice of another category is not taken.
        assert_eq!(selection("ammo", &["heavy"]).unwrap().sub_types.len(), 0);
    }

    #[test]
    fn other_is_everything_the_categories_leave() {
        let other = selection(OTHER, &[]).unwrap();
        assert!(other.any_of.is_empty());
        let personal = other.none_of.iter().find(|(item_type, _)| item_type == "WeaponPersonal").unwrap();
        assert!(personal.1.contains(&"Small".to_string()) && !personal.1.contains(&"Other".to_string()));
        assert!(other.none_of.iter().any(|(item_type, subs)| item_type == "Char_Armor_Torso" && subs.is_empty()));
    }

    #[test]
    fn a_type_taken_whole_wins_over_its_sub_types() {
        let grouped = group_pairs([("A", "x"), ("B", ""), ("A", ""), ("B", "y")].into_iter());
        assert_eq!(grouped, [("A".to_string(), vec![]), ("B".to_string(), vec![])]);
    }

    #[test]
    fn only_what_the_game_tags_stackable_and_currency_stack() {
        assert_eq!(stack_limit("WeaponPersonal", true), 99);
        assert_eq!(stack_limit("Currency", false), 999);
        assert_eq!(stack_limit("WeaponPersonal", false), 0);
    }

    #[test]
    fn the_table_names_the_parts_and_the_weights() {
        let table = table();
        assert_eq!(table["labels"]["Char_Armor_Torso"], "Core");
        assert_eq!(table["labels"]["WeaponPersonal:Small"], "Sidearms");
        assert_eq!(table["weights"]["Heavy"]["id"], "heavy");
        assert_eq!(table["categories"].as_array().unwrap().last().unwrap()["id"], "other");
    }
}
