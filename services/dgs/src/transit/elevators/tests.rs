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
    let mut elevators = Elevators::new(outbox);
    elevators.open_hangar(PLAYER, &home, SESSION);
    (elevators, home, updates)
}

fn drain(updates: &mut UnboundedReceiver<Sent>) -> Vec<Update> {
    let mut out = Vec::new();
    while let Ok(Ok(SessionUpdate { update: Some(update) })) = updates.try_recv() {
        out.push(update);
    }
    out
}

fn bound(updates: &[Update]) -> Vec<u64> {
    updates.iter().flat_map(|update| match update {
        Update::Binds(binds) => binds.binds.iter().map(|bind| bind.entity_id).collect(),
        _ => Vec::new(),
    }).collect()
}

fn unbound(updates: &[Update]) -> Vec<u64> {
    updates.iter().flat_map(|update| match update {
        Update::Unbinds(unbinds) => unbinds.entity_ids.clone(),
        _ => Vec::new(),
    }).collect()
}

#[test]
fn every_gateway_of_orisons_hangar_has_its_panel_door_and_limbo_node() {
    let (elevators, home, _) = orison();
    let hangar = &elevators.hangars[&PLAYER];
    assert_eq!(hangar.gateways.len(), 4);
    let mut panels: Vec<u64> = hangar.gateways.iter().filter_map(|gateway| gateway.panel).map(|panel| panel - home.entity_id).collect();
    panels.sort_unstable();
    assert_eq!(panels, [238, 242, 246, 250]);
    assert!(hangar.gateways.iter().all(|gateway| gateway.door.is_some() && gateway.door_bind.is_some()));
    assert!(hangar.gateways.iter().all(|gateway| gateway.limbo.position[2] < gateway.transform.position[2] - 5.0), "below the door");
}

#[test]
fn a_call_brings_a_carriage_that_opens_the_door_and_goes_when_unused() {
    let (mut elevators, home, mut updates) = orison();
    let gateway = &elevators.hangars[&PLAYER].gateways[2];
    let (panel, door) = (gateway.panel.unwrap(), gateway.door.unwrap());
    assert!(!elevators.call(PLAYER, PLAYER, home.entity_id), "the hangar is no panel");
    assert!(elevators.call(PLAYER, PLAYER, panel));
    let start = Instant::now();
    let mut seen = Vec::new();
    for step in 0..=400 {
        elevators.step(start + Duration::from_millis(step * 500));
        seen.extend(drain(&mut updates));
    }
    let carriages: Vec<u64> = bound(&seen).into_iter().filter(|id| *id >= FIRST_CARRIAGE_ID).collect();
    assert!(!carriages.is_empty(), "a carriage is bound to the hangar's client");
    assert!(bound(&seen).contains(&door) && unbound(&seen).contains(&door), "the door is bound again, opened then closed");
    assert!(unbound(&seen).contains(&carriages[0]), "and the carriage goes once unused");
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
    assert!(elevators.carriages().iter().all(|(.., shown)| shown.is_none()));
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
    let batches: Vec<EntityBinds> = seen.iter().filter_map(|update| match update {
        Update::Binds(binds) => Some(binds.clone()),
        _ => None,
    }).collect();
    let carriage_batch = batches.iter().find(|batch| batch.key.starts_with("carriage:")).expect("the carriage is bound");
    let carriage = carriage_batch.binds[0].entity_id;
    assert_eq!(carriage_batch.binds.len(), 13, "the carriage and its cab's 12 entities");
    assert!(carriage_batch.binds[1..].iter().all(|bind| bind.parent_id == carriage && bind.entity_id > carriage && bind.entity_id <= carriage + 12));
    let cab_panel = carriage_batch.binds.iter().find(|bind| bind.entity_id == carriage + 8).unwrap();
    let list = &cab_panel.snapshots.iter().find(|snapshot| snapshot.key == 0x2156_4e7d).expect("panelInteractions").value;
    assert!(list[0] >= 2, "at least the hangar's stop and the doors");
    let door_batches: Vec<&EntityBinds> = batches.iter().filter(|batch| batch.key == format!("door:{}", carriage + 11)).collect();
    assert_eq!(door_batches.len(), 1, "the cab door, bound again open");
    // The hangar's stop is the last stop, before the doors.
    let points: Vec<(u32, Option<Stop>)> = elevators.lines.values().flat_map(|run| run.carriages.values().flat_map(|entity| entity.points.clone())).collect();
    let (hangar_point, _) = points.iter().find(|(_, stop)| *stop == Some(Stop::Hangar(PLAYER))).unwrap();
    assert!(elevators.cab_panel_press(PLAYER, carriage + 8, wire::OPEN_DOORS_POINT));
    assert!(elevators.cab_panel_press(PLAYER, carriage + 8, *hangar_point));
    assert!(!elevators.cab_panel_press(PLAYER, carriage + 8, 0x1234), "a point it does not list");
    assert!(!elevators.cab_panel_press(PLAYER, carriage + 9, *hangar_point), "not the panel");
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
    assert!(!unbound(&seen).contains(&carriage), "never taken from under its rider");
    assert_eq!(elevators.carriage_zones(), [carriage]);
    elevators.observe(&[(PLAYER, home.entity_id)]);
    for step in 601..=900 {
        elevators.step(start + Duration::from_secs(step));
    }
    assert!(unbound(&drain(&mut updates)).contains(&carriage), "unused again, it goes");
}
