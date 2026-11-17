use std::time::Duration;

use sc_api::internal::SessionUpdate;
use tokio::sync::mpsc::UnboundedReceiver;

use super::*;
use crate::{location::HomeHangar, outbox::Sent};

/// Orison's spaceport, whose small front hangar has four gateways served by `TransitManager_Spaceport_to_Hangars`.
const ORISON: u32 = 1_105_811_199;
const SESSION: SessionKey = (1, 7);
const PLAYER: PlayerId = 300_000_000_001;

fn orison() -> (Elevators, HomeHangar, UnboundedReceiver<Sent>) {
    let outbox = Outbox::default();
    let updates = outbox.subscribe(SESSION);
    let home = HomeHangar::new(ORISON, 1, true).unwrap();
    let mut elevators = Elevators::new(outbox).with_cab_statics(wire::CabStatics::Server);
    elevators.open_hangar(PLAYER, &home, SESSION);
    (elevators, home, updates)
}

/// The ids a carriage bind lists in `tcOpenedDoors`.
fn opened_of(bind: &sc_api::internal::EntityBind) -> Vec<u64> {
    let value = &bind
        .snapshots
        .iter()
        .find(|snapshot| snapshot.key == 0x1e97_d055)
        .expect("tcOpenedDoors")
        .value;
    value[1..]
        .chunks(8)
        .map(|id| u64::from_le_bytes(id.try_into().unwrap()))
        .collect()
}

fn drain(updates: &mut UnboundedReceiver<Sent>) -> Vec<Update> {
    let mut out = Vec::new();
    while let Ok(Ok(SessionUpdate {
        update: Some(update),
    })) = updates.try_recv()
    {
        out.push(update);
    }
    out
}

fn bound(updates: &[Update]) -> Vec<u64> {
    updates
        .iter()
        .flat_map(|update| match update {
            Update::Binds(binds) => binds.binds.iter().map(|bind| bind.entity_id).collect(),
            _ => Vec::new(),
        })
        .collect()
}

fn unbound(updates: &[Update]) -> Vec<u64> {
    updates
        .iter()
        .flat_map(|update| match update {
            Update::Unbinds(unbinds) => unbinds.entity_ids.clone(),
            _ => Vec::new(),
        })
        .collect()
}

#[test]
fn every_gateway_of_orisons_hangar_has_its_panel_door_and_limbo_node() {
    let (elevators, home, _) = orison();
    let hangar = &elevators.hangars[&PLAYER];
    assert_eq!(hangar.gateways.len(), 4);
    let mut panels: Vec<u64> = hangar
        .gateways
        .iter()
        .filter_map(|gateway| gateway.panel)
        .map(|panel| panel - home.entity_id)
        .collect();
    panels.sort_unstable();
    assert_eq!(panels, [238, 242, 246, 250]);
    assert!(hangar
        .gateways
        .iter()
        .all(|gateway| gateway.door.is_some() && gateway.door_bind.is_some()));
    assert!(
        hangar
            .gateways
            .iter()
            .all(|gateway| gateway.limbo.position[2] < gateway.transform.position[2] - 5.0),
        "below the door"
    );
}

#[test]
fn a_call_brings_a_carriage_that_opens_the_door_and_goes_when_unused() {
    let (mut elevators, home, mut updates) = orison();
    let gateway = &elevators.hangars[&PLAYER].gateways[2];
    let (panel, door) = (gateway.panel.unwrap(), gateway.door.unwrap());
    assert!(
        !elevators.call(PLAYER, PLAYER, home.entity_id),
        "the hangar is no panel"
    );
    assert!(elevators.call(PLAYER, PLAYER, panel));
    let start = Instant::now();
    let mut seen = Vec::new();
    for step in 0..=400 {
        elevators.step(start + Duration::from_millis(step * 500));
        seen.extend(drain(&mut updates));
    }
    let carriages: Vec<u64> = bound(&seen)
        .into_iter()
        .filter(|id| *id >= FIRST_CARRIAGE_ID)
        .collect();
    assert!(
        !carriages.is_empty(),
        "a carriage is bound to the hangar's client"
    );
    assert!(bound(&seen).contains(&door) && unbound(&seen).contains(&door), "the gateway's door is bound again with its state, opened then closed, and is on the carriage's list too");
    let lists: Vec<Vec<u64>> = seen
        .iter()
        .filter_map(|update| match update {
            Update::Binds(binds) => binds
                .binds
                .iter()
                .find(|bind| bind.entity_id == carriages[0])
                .map(opened_of),
            _ => None,
        })
        .collect();
    assert!(
        lists
            .iter()
            .any(|list| list.contains(&door) && list.windows(2).all(|pair| pair[0] < pair[1])),
        "opened with the gateway's door, ascending"
    );
    assert_eq!(
        lists.last().unwrap(),
        &Vec::<u64>::new(),
        "and closed with none"
    );
    assert!(
        unbound(&seen).contains(&carriages[0]),
        "and the carriage goes once unused"
    );
    assert!(elevators.carriages().is_empty());
}

#[test]
fn a_hangar_that_closes_takes_its_carriages_from_its_client() {
    let (mut elevators, _, mut updates) = orison();
    let panel = elevators.hangars[&PLAYER].gateways[0].panel.unwrap();
    elevators.call(PLAYER, PLAYER, panel);
    elevators.step(Instant::now() + Duration::from_secs(1));
    assert!(!elevators.carriages().is_empty());
    elevators.close_hangar(PLAYER);
    drain(&mut updates);
    assert_eq!(elevators.owner_of(SESSION), None);
    assert!(elevators
        .carriages()
        .iter()
        .all(|(.., shown)| shown.is_none()));
    assert!(!elevators.call(PLAYER, PLAYER, panel));
}

/// Orison's cab has 12 entities, bound with the carriage after its id: its door at index 10, opened by binding it again
/// with its state, and its panel at index 7, which lists the line's stops, the hangar's and the doors.
#[test]
fn a_carriage_comes_with_its_cab_whose_panel_picks_stops() {
    let (mut elevators, _, mut updates) = orison();
    let panel = elevators.hangars[&PLAYER].gateways[1].panel.unwrap();
    elevators.call(PLAYER, PLAYER, panel);
    elevators.step(Instant::now() + Duration::from_secs(1));
    let seen = drain(&mut updates);
    let batches: Vec<EntityBinds> = seen
        .iter()
        .filter_map(|update| match update {
            Update::Binds(binds) => Some(binds.clone()),
            _ => None,
        })
        .collect();
    let carriage_batch = batches
        .iter()
        .find(|batch| batch.key.starts_with("carriage:"))
        .expect("the carriage is bound");
    let carriage = carriage_batch.binds[0].entity_id;
    assert_eq!(
        carriage_batch.binds.len(),
        13,
        "the carriage and its cab's 12 entities"
    );
    assert!(carriage_batch.binds[1..]
        .iter()
        .all(|bind| bind.parent_id == carriage
            && bind.entity_id > carriage
            && bind.entity_id <= carriage + 12));
    let cab_panel = carriage_batch
        .binds
        .iter()
        .find(|bind| bind.entity_id == carriage + 8)
        .unwrap();
    let list = &cab_panel
        .snapshots
        .iter()
        .find(|snapshot| snapshot.key == 0x2156_4e7d)
        .expect("panelInteractions")
        .value;
    assert!(list[0] >= 2, "at least the hangar's stop and the doors");
    let door_batches: Vec<&EntityBinds> = batches
        .iter()
        .filter(|batch| batch.key == format!("door:{}", carriage + 11))
        .collect();
    assert_eq!(door_batches.len(), 1, "the cab door, bound again open");
    // The hangar's stop is the last stop, before the doors.
    let points: Vec<(u32, Option<Stop>)> = elevators
        .lines
        .values()
        .flat_map(|run| {
            run.carriages
                .values()
                .flat_map(|entity| entity.points.clone())
        })
        .collect();
    let (hangar_point, _) = points
        .iter()
        .find(|(_, stop)| *stop == Some(Stop::Hangar(PLAYER)))
        .unwrap();
    assert!(elevators.cab_panel_press(PLAYER, carriage + 8, wire::OPEN_DOORS_POINT));
    assert!(elevators.cab_panel_press(PLAYER, carriage + 8, *hangar_point));
    assert!(
        !elevators.cab_panel_press(PLAYER, carriage + 8, 0x1234),
        "a point it does not list"
    );
    assert!(
        !elevators.cab_panel_press(PLAYER, carriage + 9, *hangar_point),
        "not the panel"
    );
}

/// A player whose zone is the carriage rides it: the carriage stays past its idle time and is never bound again under
/// them. Once they step out, it goes when unused.
#[test]
fn a_rider_keeps_the_carriage_until_they_step_out() {
    let (mut elevators, home, mut updates) = orison();
    let panel = elevators.hangars[&PLAYER].gateways[2].panel.unwrap();
    elevators.call(PLAYER, PLAYER, panel);
    let start = Instant::now();
    elevators.step(start + Duration::from_secs(1));
    let carriage = elevators.carriage_zones()[0];
    elevators.observe(&[(PLAYER, carriage)]);
    drain(&mut updates);
    for step in 2..=600 {
        elevators.step(start + Duration::from_secs(step));
    }
    let seen = drain(&mut updates);
    assert!(
        !unbound(&seen).contains(&carriage),
        "never taken from under its rider"
    );
    assert_eq!(elevators.carriage_zones(), [carriage]);
    elevators.observe(&[(PLAYER, home.entity_id)]);
    for step in 601..=900 {
        elevators.step(start + Duration::from_secs(step));
    }
    assert!(
        unbound(&drain(&mut updates)).contains(&carriage),
        "unused again, it goes"
    );
}

/// The gateway's door is bound with `autoSerializeState` off, so it follows the carriage's `tcOpenedDoors` only.
#[test]
fn a_gateway_door_follows_the_carriages_open_doors_only() {
    let home = HomeHangar::new(ORISON, 1, true).unwrap();
    let binds = home.content_binds();
    let gateway_doors: Vec<&crate::entity::Bind> = binds
        .iter()
        .filter(|bind| crate::location::DOOR_CLASSES.contains(&bind.class_crc))
        .collect();
    assert!(!gateway_doors.is_empty());
    for bind in gateway_doors {
        let snapshot = bind
            .snapshots
            .iter()
            .find(|snapshot| snapshot.key == 0xba05_30e2)
            .expect("autoSerializeState");
        assert_eq!(
            (snapshot.type_id, snapshot.value.clone()),
            (0x0b9f_c026, vec![0])
        );
    }
}

/// With the indices set, the doors change in place: the cab's doors animate (opening then opened, closing then closed),
/// the carriage's open doors are written ascending with the gateway's, and nothing is bound again.
#[test]
fn with_indices_the_doors_change_in_place_and_animate() {
    let outbox = Outbox::default();
    let mut updates = outbox.subscribe(SESSION);
    let home = HomeHangar::new(ORISON, 1, true).unwrap();
    let mut elevators = Elevators::with_indices(outbox, wire::Indices::ASSUMED);
    elevators.open_hangar(PLAYER, &home, SESSION);
    let gateway = &elevators.hangars[&PLAYER].gateways[2];
    let (panel, door) = (gateway.panel.unwrap(), gateway.door.unwrap());
    elevators.call(PLAYER, PLAYER, panel);
    let start = Instant::now();
    let mut seen = Vec::new();
    for step in 0..=400 {
        elevators.step(start + Duration::from_millis(step * 500));
        seen.extend(drain(&mut updates));
    }
    let carriage = bound(&seen)
        .into_iter()
        .find(|id| *id >= FIRST_CARRIAGE_ID)
        .unwrap();
    let binds_of_carriage = seen.iter().filter(|update| matches!(update, Update::Binds(binds) if binds.binds.iter().any(|bind| bind.entity_id == carriage))).count();
    assert_eq!(binds_of_carriage, 1, "the carriage is bound once");
    let variables: Vec<&EntityVariables> = seen
        .iter()
        .filter_map(|update| match update {
            Update::Variables(variables) => Some(variables),
            _ => None,
        })
        .collect();
    let cab_door = carriage + 11;
    let states: Vec<u8> = variables
        .iter()
        .filter(|variables| variables.entity_id == cab_door)
        .map(|variables| {
            assert_eq!(variables.variables[0].index, 10);
            variables.variables[0].value[0]
        })
        .collect();
    assert_eq!(
        states,
        [5, 3, 2, 0],
        "opening then opened, closing then closed"
    );
    let lists: Vec<Vec<u8>> = variables
        .iter()
        .filter(|variables| variables.entity_id == carriage)
        .map(|variables| variables.variables[0].value.clone())
        .collect();
    assert_eq!(lists.len(), 2);
    let ids: Vec<u64> = lists[0][1..]
        .chunks(8)
        .map(|id| u64::from_le_bytes(id.try_into().unwrap()))
        .collect();
    assert!(ids.contains(&door) && ids.contains(&cab_door));
    let sorted = {
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted
    };
    assert_eq!(ids, sorted);
    assert_eq!(lists[1], [0], "an empty list closes them");
    let panel_updates = variables
        .iter()
        .filter(|variables| {
            variables.entity_id == carriage + 8 && variables.variables[0].index == 6
        })
        .count();
    assert_eq!(
        panel_updates, 1,
        "the panel's list is sent again in place once"
    );
}

/// With `CabStatics::Client` the carriage is bound alone with the cab's `ocIdRange` (its lights load from the `.soc`), the
/// cab's doors stay in `tcOpenedDoors`, and nothing of the cab is bound or unbound by the server.
#[test]
fn with_client_cab_statics_only_the_carriage_is_bound_and_it_carries_the_cabs_range() {
    let outbox = Outbox::default();
    let mut updates = outbox.subscribe(SESSION);
    let home = HomeHangar::new(ORISON, 1, true).unwrap();
    let mut elevators = Elevators::with_indices(outbox, wire::Indices::default())
        .with_cab_statics(wire::CabStatics::Client);
    elevators.open_hangar(PLAYER, &home, SESSION);
    let panel = elevators.hangars[&PLAYER].gateways[2].panel.unwrap();
    elevators.call(PLAYER, PLAYER, panel);
    let start = Instant::now();
    let mut seen = Vec::new();
    for step in 0..=30 {
        elevators.step(start + Duration::from_millis(step * 500));
        seen.extend(drain(&mut updates));
    }
    let carriage = bound(&seen)
        .into_iter()
        .find(|id| *id >= FIRST_CARRIAGE_ID)
        .unwrap();
    let cab = elevators.lines.values().next().unwrap().cab.len() as u64;
    assert!(cab > 0);
    let panel_id = carriage + wire::CAB_IDS;
    assert!(
        cab < wire::CAB_IDS,
        "the panel's id is past the client's range"
    );
    let others: Vec<u64> = bound(&seen)
        .into_iter()
        .filter(|id| *id != carriage && *id != panel_id && *id >= FIRST_CARRIAGE_ID)
        .collect();
    assert!(
        others
            .iter()
            .all(|id| *id < carriage + 1 || *id > carriage + cab),
        "no other cab entity is bound by the server"
    );
    assert!(
        bound(&seen).contains(&panel_id),
        "the server's cab panel is bound outside the range"
    );
    assert!(
        unbound(&seen)
            .iter()
            .all(|id| *id < carriage + 1 || *id > carriage + cab),
        "nothing in the range is unbound"
    );
    let panel_bind = seen
        .iter()
        .find_map(|update| match update {
            Update::Binds(binds) => binds.binds.iter().find(|bind| bind.entity_id == panel_id),
            _ => None,
        })
        .unwrap();
    assert_eq!(panel_bind.parent_id, carriage);
    assert!(
        panel_bind
            .snapshots
            .iter()
            .any(|snapshot| snapshot.key == 0x2156_4e7d),
        "it lists the stops"
    );
    let point = elevators
        .lines
        .values()
        .next()
        .unwrap()
        .carriages
        .values()
        .find(|entity| entity.entity_id == carriage)
        .unwrap()
        .points[0]
        .0;
    assert!(
        elevators.cab_panel_press(PLAYER, panel_id, point),
        "a press on it reaches the line"
    );
    assert!(
        !elevators.cab_panel_press(PLAYER, carriage + 1, point),
        "the client's own copy is not ours"
    );
    let bind = seen
        .iter()
        .find_map(|update| match update {
            Update::Binds(binds) => binds.binds.iter().find(|bind| bind.entity_id == carriage),
            _ => None,
        })
        .expect("the carriage is bound");
    let range = bind
        .snapshots
        .iter()
        .find(|snapshot| snapshot.key == 0xe359_14ad)
        .expect("ocIdRange");
    assert_eq!(
        range.value,
        [(carriage + 1).to_le_bytes(), (carriage + cab).to_le_bytes()].concat()
    );
    assert!(opened_of(bind).iter().all(|id| *id != 0));
}

/// With `CabStatics::Merge` the carriage carries the cab's range as with `Client`, and the cab panel is bound inside it, at
/// its own static's id, as the carriage's content (`0x1800`) so the client merges the `.soc` into it. It lists the stops and a
/// press on it reaches the line.
#[test]
fn with_merged_cab_statics_the_panel_is_bound_at_its_static_id_as_the_carriages_content() {
    let outbox = Outbox::default();
    let mut updates = outbox.subscribe(SESSION);
    let home = HomeHangar::new(ORISON, 1, true).unwrap();
    let mut elevators = Elevators::with_indices(outbox, wire::Indices::default())
        .with_cab_statics(wire::CabStatics::Merge);
    elevators.open_hangar(PLAYER, &home, SESSION);
    let panel = elevators.hangars[&PLAYER].gateways[2].panel.unwrap();
    elevators.call(PLAYER, PLAYER, panel);
    let start = Instant::now();
    let mut seen = Vec::new();
    for step in 0..=30 {
        elevators.step(start + Duration::from_millis(step * 500));
        seen.extend(drain(&mut updates));
    }
    let carriage = bound(&seen)
        .into_iter()
        .find(|id| *id >= FIRST_CARRIAGE_ID)
        .unwrap();
    let run = elevators.lines.values().next().unwrap();
    let cab = run.cab.len() as u64;
    let index = run
        .cab
        .iter()
        .find(|entity| entity.class_crc == CAB_PANEL_CLASS)
        .unwrap()
        .index;
    let panel_id = carriage + 1 + index;
    assert!(panel_id <= carriage + cab, "inside the client's range");
    let ids: Vec<u64> = seen
        .iter()
        .filter_map(|update| match update {
            Update::Binds(binds) => Some(
                binds
                    .binds
                    .iter()
                    .map(|bind| bind.entity_id)
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect();
    let order = |id: u64| ids.iter().position(|bound| *bound == id).unwrap();
    assert!(
        order(carriage) < order(panel_id),
        "the carriage is pending when the panel is added"
    );
    assert!(
        ids.iter()
            .filter(|id| **id > carriage && **id <= carriage + cab)
            .all(|id| *id == panel_id),
        "only the panel is bound in the range"
    );
    let panel_bind = seen
        .iter()
        .find_map(|update| match update {
            Update::Binds(binds) => binds.binds.iter().find(|bind| bind.entity_id == panel_id),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (panel_bind.flags, panel_bind.parent_id),
        (crate::location::MERGED_STATIC_FLAGS, carriage)
    );
    assert!(
        panel_bind
            .snapshots
            .iter()
            .any(|snapshot| snapshot.key == 0x2156_4e7d),
        "it lists the stops"
    );
    let point = elevators
        .lines
        .values()
        .next()
        .unwrap()
        .carriages
        .values()
        .find(|entity| entity.entity_id == carriage)
        .unwrap()
        .points[0]
        .0;
    assert!(
        elevators.cab_panel_press(PLAYER, panel_id, point),
        "a press on it reaches the line"
    );
    let bind = seen
        .iter()
        .find_map(|update| match update {
            Update::Binds(binds) => binds.binds.iter().find(|bind| bind.entity_id == carriage),
            _ => None,
        })
        .expect("the carriage is bound");
    assert!(
        bind.snapshots
            .iter()
            .any(|snapshot| snapshot.key == 0xe359_14ad),
        "ocIdRange"
    );
}

#[test]
fn an_owner_holds_the_home_hangar_and_an_instance_as_two_stops() {
    let (mut elevators, _, _updates) = orison();
    let instance = HomeHangar::with_id(ORISON, crate::hangar_slots::POOL_FIRST_ID, true).unwrap();
    let stop = elevators
        .open_instance(PLAYER, &instance, SESSION)
        .expect("a Transit hangar");
    assert_eq!(stop, instance.entity_id);
    assert_eq!(elevators.stops_of(SESSION), [stop, PLAYER]);
    assert_eq!(
        elevators.owner_of(SESSION),
        Some(PLAYER),
        "the home stop, not the instance"
    );
    let panel = elevators.hangars[&stop].gateways[0].panel.unwrap();
    assert_eq!(elevators.stop_of_panel(panel), Some(stop));
    elevators.close_hangar(stop);
    assert_eq!(
        elevators.stops_of(SESSION),
        [PLAYER],
        "closing the instance leaves the home hangar"
    );
    assert!(elevators.call(
        PLAYER,
        PLAYER,
        elevators.hangars[&PLAYER].gateways[0].panel.unwrap()
    ));
}

/// The manager of Orison's hangar elevator.
fn orison_manager(elevators: &Elevators) -> Owner {
    elevators.hangars[&PLAYER].manager.clone()
}

#[test]
fn a_panel_lists_the_fixed_stops_then_the_hangars_then_the_doors() {
    let (mut elevators, _, _updates) = orison();
    elevators.set_player_name(PLAYER, "Gaussian");
    let manager = orison_manager(&elevators);
    let run = &elevators.lines[&manager];
    let list = panel_entries(run, &elevators.listed_for(&manager, PLAYER), Some(PLAYER));
    let fixed = run.destinations.len();
    assert!(fixed > 0, "the manager has destinations of its own");
    assert_eq!(
        list.entries.len(),
        fixed + 2,
        "its fixed stops, the player's hangar and the doors"
    );
    for (position, destination) in run.destinations.iter().enumerate() {
        let (index, flags) = (
            destination.index,
            if destination.important {
                wire::PANEL_IMPORTANT
            } else {
                0
            } | wire::PANEL_ENABLED,
        );
        assert_eq!(
            (list.entries[position].point, list.entries[position].flags),
            (wire::destination_point(index), flags)
        );
        assert_eq!(
            list.points[position],
            (
                wire::destination_point(index),
                Some(Stop::Destination(index))
            )
        );
    }
    let hangar = &list.entries[fixed];
    assert_eq!(
        (hangar.label.as_str(), hangar.point, hangar.flags),
        (
            "@ui_interactor_hangar_small",
            wire::stop_point(0),
            wire::PANEL_PRIMARY | wire::PANEL_ENABLED
        )
    );
    assert_eq!(
        list.points[fixed],
        (wire::stop_point(0), Some(Stop::Hangar(PLAYER)))
    );
    assert_eq!(
        (list.entries[fixed + 1].point, list.points[fixed + 1]),
        (wire::OPEN_DOORS_POINT, (wire::OPEN_DOORS_POINT, None))
    );
    assert_eq!(
        list.names.len(),
        fixed + 1,
        "a name per stop, none for the doors"
    );
    assert_eq!(
        (
            list.names[fixed].owner.as_str(),
            list.names[0].owner.as_str()
        ),
        ("Gaussian", "")
    );
}

#[test]
fn a_panel_names_its_current_floor_and_marks_the_lobby_important() {
    let (elevators, _, _updates) = orison();
    let manager = orison_manager(&elevators);
    let run = &elevators.lines[&manager];
    let list = panel_entries(run, &elevators.listed_for(&manager, PLAYER), Some(PLAYER));
    let lobbies: Vec<_> = run
        .destinations
        .iter()
        .filter(|destination| destination.important)
        .collect();
    assert!(
        !lobbies.is_empty(),
        "the data marks a lobby on Orison's line"
    );
    let important = list
        .entries
        .iter()
        .filter(|entry| entry.flags & wire::PANEL_IMPORTANT != 0)
        .count();
    assert_eq!(
        (
            important,
            list.names.iter().filter(|name| name.important).count()
        ),
        (lobbies.len(), lobbies.len())
    );
    assert_eq!(
        list.current as usize,
        run.destinations.len(),
        "the current floor is the player's hangar, after the fixed stops"
    );
    let shown = list.snapshots(&run.name);
    let value = |key: u32| {
        shown
            .iter()
            .find(|snapshot| snapshot.key == key)
            .map(|snapshot| snapshot.value.clone())
    };
    assert_eq!(
        value(0x5e3e_3533),
        Some(list.current.to_le_bytes().to_vec()),
        "tdpCurrentStop"
    );
    assert_eq!(
        value(0xbd23_a6c5),
        Some((list.names.len() as u32).to_le_bytes().to_vec()),
        "tdpNumUnlockLocs"
    );
    assert_eq!(
        value(0x82c5_7d34),
        None,
        "tdpOverrideStyleGUID is not sent unless SC_DGS_PANEL_STYLE=live"
    );
}

#[test]
fn with_the_current_stop_index_an_arrival_writes_the_floor_in_place() {
    let outbox = Outbox::default();
    let mut updates = outbox.subscribe(SESSION);
    let home = HomeHangar::new(ORISON, 1, true).unwrap();
    let indices = wire::Indices::parse(Some("10,6,6,30"));
    let mut elevators = Elevators::with_indices(outbox, indices);
    elevators.open_hangar(PLAYER, &home, SESSION);
    let call = elevators.hangars[&PLAYER].gateways[1].panel.unwrap();
    elevators.call(PLAYER, PLAYER, call);
    elevators.step(Instant::now() + Duration::from_secs(1));
    let _ = drain(&mut updates);
    let manager = orison_manager(&elevators);
    let fixed = elevators.lines[&manager].destinations.len() as u32;
    let panel = elevators.lines[&manager].carriages.values().next().and_then(|entity| elevators.lines[&manager].panel_id(entity.entity_id));
    elevators.arrived(&manager, *elevators.lines[&manager].carriages.keys().next().unwrap());
    let sent = drain(&mut updates);
    let write = sent.iter().find_map(|update| match update {
        Update::Variables(variables) if Some(variables.entity_id) == panel => Some(variables.variables[0].clone()),
        _ => None,
    });
    let write = write.expect("the panel's current stop is written");
    assert_eq!((write.index, write.value), (30, fixed.to_le_bytes().to_vec()));
    assert_eq!(wire::Indices::parse(Some("assumed")).current_stop, None, "no index is assumed");
}

#[test]
fn a_panel_lists_an_owners_hangars_and_a_guests_only_when_the_instance_manager_grants_it() {
    let (mut elevators, _, _updates) = orison();
    let guest = PLAYER + 1;
    let guest_session = (1, 8);
    let guest_home = HomeHangar::new(ORISON, 2, true).unwrap();
    elevators.open_hangar(guest, &guest_home, guest_session);
    elevators.set_player_name(PLAYER, "Gaussian");
    let instance = HomeHangar::with_id(ORISON, crate::hangar_slots::POOL_FIRST_ID, true).unwrap();
    let stop = elevators
        .open_instance(PLAYER, &instance, SESSION)
        .expect("a Transit hangar");
    let manager = orison_manager(&elevators);
    let stops = |elevators: &Elevators, viewer| {
        elevators
            .listed_for(&manager, viewer)
            .into_iter()
            .map(|hangar| (hangar.stop, hangar.owner))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        stops(&elevators, PLAYER),
        [
            (PLAYER, "Gaussian".to_string()),
            (stop, "Gaussian".to_string())
        ],
        "their home first, then the instance"
    );
    assert_eq!(
        stops(&elevators, guest),
        [(guest, wire::UNKNOWN_OWNER.to_string())],
        "a stranger's hangar is never offered"
    );
    assert!(elevators.allow_guest(stop, guest));
    assert_eq!(
        stops(&elevators, guest),
        [
            (guest, wire::UNKNOWN_OWNER.to_string()),
            (stop, "Gaussian".to_string())
        ]
    );
    assert!(!elevators.allow_guest(1, guest), "a stop that is not open");
    elevators.deny_guest(stop, guest);
    assert_eq!(stops(&elevators, guest).len(), 1);
}

#[test]
fn a_hangars_label_follows_its_size() {
    assert_eq!(
        [
            "Hangar_SmallFront_Orison",
            "Hangar_MediumTop_Lorville",
            "Hangar_LargeTop_Pyro",
            "Hangar_XLTop_Pyro",
            "Something"
        ]
        .map(hangar_label),
        [
            "@ui_interactor_hangar_small",
            "@ui_interactor_hangar_medium",
            "@ui_interactor_hangar_large",
            "@ui_interactor_hangar_xlarge",
            "@ui_interactor_hangar_small"
        ]
    );
}

#[test]
fn the_cab_panel_is_bound_with_the_official_snapshots() {
    let (mut elevators, _, mut updates) = orison();
    let panel = elevators.hangars[&PLAYER].gateways[1].panel.unwrap();
    elevators.call(PLAYER, PLAYER, panel);
    elevators.step(Instant::now() + Duration::from_secs(1));
    let seen = drain(&mut updates);
    let batch = seen
        .iter()
        .find_map(|update| match update {
            Update::Binds(binds) if binds.key.starts_with("carriage:") => Some(binds.clone()),
            _ => None,
        })
        .expect("the carriage is bound");
    let carriage = batch.binds[0].entity_id;
    let cab_panel = batch
        .binds
        .iter()
        .find(|bind| bind.entity_id == carriage + 8)
        .unwrap();
    let keys: Vec<u32> = cab_panel
        .snapshots
        .iter()
        .map(|snapshot| snapshot.key)
        .collect();
    for key in [0x2156_4e7d_u32, 0xe924_f32c, 0xed2d_5e3d] {
        assert!(keys.contains(&key), "{key:#x} is bound with the panel");
    }
}

#[test]
fn a_pick_of_a_fixed_stop_is_dropped_until_this_dgs_serves_it() {
    let (mut elevators, _, mut updates) = orison();
    let panel = elevators.hangars[&PLAYER].gateways[1].panel.unwrap();
    elevators.call(PLAYER, PLAYER, panel);
    elevators.step(Instant::now() + Duration::from_secs(1));
    let _ = drain(&mut updates);
    let (carriage, destination) = {
        let run = elevators.lines.values().next().unwrap();
        (
            run.carriages.values().next().unwrap().entity_id,
            run.destinations[0].index,
        )
    };
    assert!(
        elevators.cab_panel_press(PLAYER, carriage + 8, wire::destination_point(destination)),
        "the press is read"
    );
}
