use std::collections::{BTreeMap, BTreeSet};

use super::*;

/// The lobby (three gateways), a docking port (one) and the persistent stop is the lobby. Hangars are opened per test.
fn spec(fleet: usize) -> LineSpec {
    LineSpec {
        system: System::Transit,
        fleet,
        destinations: vec![DestinationSpec { gateways: vec![0, 1, 2], location: 100 }, DestinationSpec { gateways: vec![3], location: 101 }],
        persistent: 0,
    }
}

const LOBBY: Stop = Stop::Destination(0);
const PORT: Stop = Stop::Destination(1);

fn lobby(gateway: usize) -> GatewayRef {
    GatewayRef { stop: LOBBY, gateway }
}

fn hangar(owner: PlayerId) -> GatewayRef {
    GatewayRef { stop: Stop::Hangar(owner), gateway: 0 }
}

struct Clock(Instant);

impl Clock {
    fn at(&self, seconds: f64) -> Instant {
        self.0 + Duration::from_secs_f64(seconds)
    }
}

fn setup(fleet: usize, settings: Settings) -> (Line, Clock) {
    let mut line = Line::new(spec(fleet), settings);
    let clock = Clock(Instant::now());
    for owner in 1..=3 {
        line.handle(clock.at(0.0), Event::HangarOpened { owner, gateways: 1 });
    }
    (line, clock)
}

fn opened(commands: &[Command]) -> Vec<(CarriageId, GatewayRef)> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::OpenDoors { carriage, gateway } => Some((*carriage, *gateway)),
            _ => None,
        })
        .collect()
}

fn created(commands: &[Command]) -> Vec<(CarriageId, Place)> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::Create { carriage, place } => Some((*carriage, *place)),
            _ => None,
        })
        .collect()
}

/// Ticks every 100 ms from `from` to `to`, both included, and gathers the commands.
fn run(line: &mut Line, clock: &Clock, from: f64, to: f64) -> Vec<(f64, Command)> {
    let mut out = Vec::new();
    let mut tenth = (from * 10.0).round() as u64;
    while tenth as f64 / 10.0 <= to + 1e-9 {
        let seconds = tenth as f64 / 10.0;
        out.extend(line.tick(clock.at(seconds)).into_iter().map(|command| (seconds, command)));
        tenth += 1;
    }
    out
}

fn first_open(commands: &[(f64, Command)]) -> Option<(f64, CarriageId, GatewayRef)> {
    commands.iter().find_map(|(seconds, command)| match command {
        Command::OpenDoors { carriage, gateway } => Some((*seconds, *carriage, *gateway)),
        _ => None,
    })
}

/// A call at a lobby gateway creates a carriage there, doors open, and `players` board it at 1 s.
fn boarded(line: &mut Line, clock: &Clock, gateway: usize, players: &[PlayerId]) -> CarriageId {
    let commands = line.handle(clock.at(0.0), Event::Call { player: players[0], gateway: lobby(gateway) });
    let [(id, Place::Gateway(at))] = created(&commands)[..] else { panic!("one carriage created: {commands:?}") };
    assert_eq!(at, lobby(gateway));
    assert_eq!(opened(&commands), [(id, lobby(gateway))]);
    for player in players {
        line.handle(clock.at(1.0), Event::Board { player: *player, carriage: id });
    }
    id
}

#[test]
fn one_rider_rides_to_a_hangar_in_the_official_time() {
    let (mut line, clock) = setup(3, Settings::default());
    let id = boarded(&mut line, &clock, 0, &[1]);
    let commands = line.handle(clock.at(3.0), Event::Pick { player: 1, stop: Stop::Hangar(1) });
    assert!(commands.contains(&Command::Load { player: 1, location: Location::Hangar(1), resend: false }));
    // The doors close 5 s after the boarding and the carriage leaves 2.5 s later.
    let commands = run(&mut line, &clock, 3.1, 8.5);
    assert!(commands.contains(&(6.0, Command::CloseDoors { carriage: id, gateway: lobby(0) })));
    assert!(commands.iter().any(|(seconds, command)| *seconds == 8.5
        && matches!(command, Command::State { state: CarriageState::Travelling, target: Some(target), .. } if *target == hangar(1))));
    line.handle(clock.at(10.0), Event::Ready { player: 1, location: Location::Hangar(1) });
    // 16 s from the pick.
    let commands = run(&mut line, &clock, 10.0, 25.0);
    assert_eq!(first_open(&commands), Some((19.0, id, hangar(1))));
    assert_eq!(line.rider(1), Some((id, None)), "the pick is served");
    assert!(line.carriage(id).unwrap().queue.is_empty());
}

#[test]
fn a_stop_picked_long_ago_still_takes_the_shortest_leg() {
    let (mut line, clock) = setup(3, Settings::default());
    let id = boarded(&mut line, &clock, 0, &[1]);
    line.handle(clock.at(1.0), Event::HoldDoors { player: 1 });
    line.handle(clock.at(1.5), Event::Pick { player: 1, stop: PORT });
    line.handle(clock.at(1.5), Event::Ready { player: 1, location: Location::Persistent(101) });
    // Held: the doors close at 1 + 5 + 5 = 11 s and the carriage leaves at 13.5 s, so the leg ends at 22 s, after the
    // 17.5 s the pick alone would give.
    let commands = run(&mut line, &clock, 1.5, 30.0);
    assert_eq!(first_open(&commands), Some((22.0, id, GatewayRef { stop: PORT, gateway: 3 })));
}

#[test]
fn the_doors_wait_for_every_rider_and_a_slow_one_is_split_off_after_the_grace() {
    let (mut line, clock) = setup(3, Settings::default());
    let id = boarded(&mut line, &clock, 0, &[1, 2]);
    line.handle(clock.at(2.0), Event::Pick { player: 1, stop: Stop::Hangar(1) });
    line.handle(clock.at(5.0), Event::Ready { player: 1, location: Location::Hangar(1) });
    // The ride is over at 18 s; the grace runs from then to 38 s.
    let commands = run(&mut line, &clock, 5.0, 37.9);
    assert!(first_open(&commands).is_none(), "{commands:?}");
    assert_eq!(line.carriage(id).unwrap().state, CarriageState::WaitingInLimbo);
    let commands = line.tick(clock.at(38.0));
    let [(other, Place::Limbo(Stop::Hangar(1)))] = created(&commands)[..] else { panic!("{commands:?}") };
    assert!(commands.contains(&Command::MoveRider { player: 2, from: id, to: other }));
    assert_eq!(opened(&commands), [(id, hangar(1))], "the ready rider gets out");
    let slow = line.carriage(other).unwrap();
    assert_eq!((slow.riders, slow.queue, slow.state), (vec![2], vec![Stop::Hangar(1)], CarriageState::WaitingInLimbo));
    // The slow rider is ready later; the hangar's one gateway is free once the first carriage is gone.
    line.handle(clock.at(39.0), Event::Exit { player: 1, carriage: id });
    line.handle(clock.at(40.0), Event::Ready { player: 2, location: Location::Hangar(1) });
    let commands = run(&mut line, &clock, 40.0, 60.0);
    // Riderless, the first closes at 44 s, is idle at 46.5 s, and is removed to free the gateway.
    let open = first_open(&commands).unwrap();
    assert_eq!((open.0, open.1, open.2), (46.5, other, hangar(1)));
    assert!(commands.iter().any(|(seconds, command)| *seconds == 46.5 && *command == Command::Remove { carriage: id }));
}

#[test]
fn without_splits_the_ready_riders_wait_with_the_doors_closed() {
    let (mut line, clock) = setup(3, Settings { split: false, ..Settings::default() });
    let id = boarded(&mut line, &clock, 0, &[1, 2]);
    line.handle(clock.at(2.0), Event::Pick { player: 1, stop: PORT });
    line.handle(clock.at(3.0), Event::Ready { player: 1, location: Location::Persistent(101) });
    let commands = run(&mut line, &clock, 3.0, 100.0);
    assert!(first_open(&commands).is_none() && created(&commands.iter().map(|(_, c)| *c).collect::<Vec<_>>()).is_empty());
    let commands = line.handle(clock.at(100.0), Event::Ready { player: 2, location: Location::Persistent(101) });
    assert_eq!(opened(&commands), [(id, GatewayRef { stop: PORT, gateway: 3 })]);
}

#[test]
fn a_full_fleet_splits_into_an_idle_carriage_or_not_at_all() {
    let (mut line, clock) = setup(1, Settings::default());
    let id = boarded(&mut line, &clock, 0, &[1, 2]);
    line.handle(clock.at(2.0), Event::Pick { player: 1, stop: PORT });
    line.handle(clock.at(3.0), Event::Ready { player: 1, location: Location::Persistent(101) });
    let commands = run(&mut line, &clock, 3.0, 200.0);
    assert!(first_open(&commands).is_none(), "a fleet of one cannot split");
    assert!(commands.iter().all(|(_, command)| !matches!(command, Command::MoveRider { .. })));
    assert_eq!(line.carriages().len(), 1);
    assert_eq!(line.carriage(id).unwrap().riders, vec![1, 2]);
}

#[test]
fn a_split_takes_an_idle_carriage_before_creating_one() {
    let (mut line, clock) = setup(3, Settings::default());
    // An idle carriage at the lobby's gateway 1.
    line.handle(clock.at(0.0), Event::Call { player: 9, gateway: lobby(1) });
    let id = boarded(&mut line, &clock, 0, &[1, 2]);
    assert_eq!(id, CarriageId(1));
    line.handle(clock.at(2.0), Event::Pick { player: 1, stop: PORT });
    line.handle(clock.at(3.0), Event::Ready { player: 1, location: Location::Persistent(101) });
    let commands = run(&mut line, &clock, 3.0, 40.0);
    assert!(commands.iter().all(|(_, command)| !matches!(command, Command::Create { .. })));
    assert!(commands.iter().any(|(_, command)| *command == Command::MoveRider { player: 2, from: id, to: CarriageId(0) }));
}

#[test]
fn a_location_never_reported_loaded_is_sent_again() {
    let (mut line, clock) = setup(3, Settings::default());
    boarded(&mut line, &clock, 0, &[1]);
    line.handle(clock.at(2.0), Event::Pick { player: 1, stop: PORT });
    let commands = run(&mut line, &clock, 2.1, 125.0);
    let resends: Vec<f64> = commands
        .iter()
        .filter(|(_, command)| *command == Command::Load { player: 1, location: Location::Persistent(101), resend: true })
        .map(|(seconds, _)| *seconds)
        .collect();
    assert_eq!(resends, [62.0, 122.0]);
}

#[test]
fn a_new_pick_replaces_the_previous_one_and_its_load() {
    let (mut line, clock) = setup(3, Settings::default());
    let id = boarded(&mut line, &clock, 0, &[1]);
    line.handle(clock.at(2.0), Event::Pick { player: 1, stop: PORT });
    let commands = line.handle(clock.at(3.0), Event::Pick { player: 1, stop: Stop::Hangar(2) });
    assert!(commands.contains(&Command::Unload { player: 1, location: Location::Persistent(101) }));
    assert!(commands.contains(&Command::Load { player: 1, location: Location::Hangar(2), resend: false }));
    assert_eq!(line.carriage(id).unwrap().queue, vec![Stop::Hangar(2)]);
    assert_eq!(line.rider(1), Some((id, Some(Stop::Hangar(2)))));
    // Picking where it stands keeps the doors open instead.
    line.handle(clock.at(4.0), Event::Pick { player: 1, stop: LOBBY });
    assert!(line.carriage(id).unwrap().queue.is_empty());
    assert_eq!(line.rider(1), Some((id, None)));
    // Stops that do not exist are not queued.
    line.handle(clock.at(4.0), Event::Pick { player: 1, stop: Stop::Hangar(7) });
    line.handle(clock.at(4.0), Event::Pick { player: 1, stop: Stop::Destination(5) });
    assert!(line.carriage(id).unwrap().queue.is_empty());
}

#[test]
fn every_rider_loads_every_queued_stop_and_stops_when_nobody_waits_for_it() {
    let (mut line, clock) = setup(3, Settings::default());
    let id = boarded(&mut line, &clock, 0, &[1, 2, 3]);
    let first = line.handle(clock.at(2.0), Event::Pick { player: 1, stop: PORT });
    let second = line.handle(clock.at(2.0), Event::Pick { player: 2, stop: Stop::Hangar(2) });
    for player in [1, 2, 3] {
        assert!(first.contains(&Command::Load { player, location: Location::Persistent(101), resend: false }));
        assert!(second.contains(&Command::Load { player, location: Location::Hangar(2), resend: false }));
    }
    // Rider 1 gets out before the carriage leaves: nobody waits for the port any more.
    let commands = line.handle(clock.at(3.0), Event::Exit { player: 1, carriage: id });
    assert!(commands.contains(&Command::Unload { player: 1, location: Location::Persistent(101) }));
    assert!(commands.contains(&Command::Unload { player: 1, location: Location::Hangar(2) }));
    for player in [2, 3] {
        assert!(commands.contains(&Command::Unload { player, location: Location::Persistent(101) }));
    }
    assert_eq!(line.carriage(id).unwrap().queue, vec![Stop::Hangar(2)]);
}

#[test]
fn the_preload_limit_caps_the_stops_loaded_past_the_next() {
    let (mut line, clock) = setup(3, Settings { preload_limit: Some(0), ..Settings::default() });
    let id = boarded(&mut line, &clock, 0, &[1, 2]);
    line.handle(clock.at(2.0), Event::Pick { player: 1, stop: PORT });
    let commands = line.handle(clock.at(2.0), Event::Pick { player: 2, stop: Stop::Hangar(2) });
    assert!(commands.iter().all(|command| !matches!(command, Command::Load { .. })), "{commands:?}");
    for player in [1, 2] {
        line.handle(clock.at(3.0), Event::Ready { player, location: Location::Persistent(101) });
    }
    // At the port, the hangar is next: everyone loads it.
    let commands = run(&mut line, &clock, 3.0, 20.0);
    assert_eq!(first_open(&commands).map(|open| (open.1, open.2)), Some((id, GatewayRef { stop: PORT, gateway: 3 })));
    for player in [1, 2] {
        assert!(commands.iter().any(|(_, command)| *command == Command::Load { player, location: Location::Hangar(2), resend: false }));
    }
}

#[test]
fn an_idle_carriage_is_called_before_a_new_one_is_created_and_the_fleet_caps_them() {
    let (mut line, clock) = setup(2, Settings::default());
    line.handle(clock.at(0.0), Event::Call { player: 1, gateway: lobby(0) });
    // Nobody boards: idle at 7.5 s.
    run(&mut line, &clock, 0.0, 8.0);
    assert_eq!(line.carriage(CarriageId(0)).unwrap().state, CarriageState::Idle);
    let commands = line.handle(clock.at(10.0), Event::Call { player: 2, gateway: GatewayRef { stop: PORT, gateway: 3 } });
    assert!(created(&commands).is_empty());
    assert_eq!(line.carriage(CarriageId(0)).unwrap().call, Some(GatewayRef { stop: PORT, gateway: 3 }));
    let commands = run(&mut line, &clock, 10.1, 11.0);
    assert_eq!(first_open(&commands), Some((10.3, CarriageId(0), GatewayRef { stop: PORT, gateway: 3 })));
    // Two riders keep both carriages; a third call waits for one of them.
    line.handle(clock.at(11.0), Event::Board { player: 2, carriage: CarriageId(0) });
    let commands = line.handle(clock.at(11.0), Event::Call { player: 3, gateway: lobby(1) });
    let [(second, _)] = created(&commands)[..] else { panic!() };
    line.handle(clock.at(12.0), Event::Board { player: 3, carriage: second });
    let commands = line.handle(clock.at(12.0), Event::Call { player: 4, gateway: lobby(2) });
    assert!(created(&commands).is_empty() && opened(&commands).is_empty());
    assert_eq!(line.calls(), vec![lobby(2)]);
    // Rider 3 gets out; once the carriage is idle it goes to the call.
    line.handle(clock.at(13.0), Event::Exit { player: 3, carriage: second });
    let commands = run(&mut line, &clock, 13.0, 30.0);
    assert_eq!(first_open(&commands), Some((20.8, second, lobby(2))));
}

#[test]
fn a_carriage_unused_for_the_idle_time_is_removed_but_never_one_with_riders() {
    let (mut line, clock) = setup(3, Settings::default());
    let id = boarded(&mut line, &clock, 0, &[1]);
    line.handle(clock.at(0.0), Event::Call { player: 2, gateway: lobby(1) });
    // The riderless one closes at 5 s, is idle at 7.5 s and goes at 127.5 s; the other keeps its rider.
    let commands = run(&mut line, &clock, 0.0, 1000.0);
    let gone: Vec<(f64, CarriageId)> = commands
        .iter()
        .filter_map(|(seconds, command)| match command {
            Command::Remove { carriage } => Some((*seconds, *carriage)),
            _ => None,
        })
        .collect();
    assert_eq!(gone, [(127.5, CarriageId(1))]);
    assert_eq!(line.carriage(id).unwrap().riders, vec![1]);
}

#[test]
fn a_rider_who_leaves_the_game_stops_counting_and_is_saved_at_the_destination() {
    let (mut line, clock) = setup(3, Settings::default());
    let id = boarded(&mut line, &clock, 0, &[1, 2]);
    line.handle(clock.at(2.0), Event::Pick { player: 1, stop: PORT });
    line.handle(clock.at(3.0), Event::Ready { player: 1, location: Location::Persistent(101) });
    run(&mut line, &clock, 3.0, 25.0);
    let commands = line.handle(clock.at(25.0), Event::Disconnected { player: 2 });
    assert_eq!(commands[0], Command::Save { player: 2, stop: PORT, gateway: Some(GatewayRef { stop: PORT, gateway: 3 }) });
    assert!(commands.iter().all(|command| !matches!(command, Command::Unload { .. })), "the connection is gone");
    assert_eq!(opened(&commands), [(id, GatewayRef { stop: PORT, gateway: 3 })], "the others no longer wait");
}

#[test]
fn a_rider_bound_for_a_hangar_is_saved_at_the_persistent_stop() {
    let (mut line, clock) = setup(3, Settings::default());
    boarded(&mut line, &clock, 0, &[1]);
    line.handle(clock.at(2.0), Event::Pick { player: 1, stop: Stop::Hangar(1) });
    run(&mut line, &clock, 2.0, 20.0);
    let commands = line.handle(clock.at(20.0), Event::Disconnected { player: 1 });
    assert_eq!(commands[0], Command::Save { player: 1, stop: LOBBY, gateway: Some(lobby(0)) });
    // Its last rider gone, the carriage in limbo is idle, then removed.
    let commands = run(&mut line, &clock, 20.0, 141.0);
    assert!(commands.iter().any(|(seconds, command)| *seconds == 140.0 && matches!(command, Command::Remove { .. })));
}

#[test]
fn anyone_aboard_may_get_out_at_another_players_hangar() {
    let (mut line, clock) = setup(3, Settings::default());
    let id = boarded(&mut line, &clock, 0, &[1, 5]);
    // Player 5 has no hangar of their own here and picks player 1's.
    line.handle(clock.at(2.0), Event::Pick { player: 5, stop: Stop::Hangar(1) });
    for player in [1, 5] {
        line.handle(clock.at(3.0), Event::Ready { player, location: Location::Hangar(1) });
    }
    let commands = run(&mut line, &clock, 3.0, 30.0);
    assert_eq!(first_open(&commands).map(|open| (open.1, open.2)), Some((id, hangar(1))));
    for player in [1, 5] {
        line.handle(clock.at(30.0), Event::Exit { player, carriage: id });
    }
    assert!(line.carriage(id).unwrap().riders.is_empty());
}

#[test]
fn a_closed_hangar_is_dropped_from_every_queue() {
    let (mut line, clock) = setup(3, Settings::default());
    let id = boarded(&mut line, &clock, 0, &[1, 2]);
    line.handle(clock.at(2.0), Event::Pick { player: 1, stop: Stop::Hangar(3) });
    line.handle(clock.at(2.0), Event::Pick { player: 2, stop: PORT });
    line.handle(clock.at(4.0), Event::HangarClosed { owner: 3 });
    assert_eq!(line.carriage(id).unwrap().queue, vec![PORT]);
    assert_eq!(line.rider(1), Some((id, None)));
}

#[test]
fn riders_at_a_hangar_that_closes_are_taken_to_the_lobby() {
    let (mut line, clock) = setup(3, Settings::default());
    let id = boarded(&mut line, &clock, 0, &[1, 5]);
    line.handle(clock.at(2.0), Event::Pick { player: 1, stop: Stop::Hangar(1) });
    for player in [1, 5] {
        line.handle(clock.at(3.0), Event::Ready { player, location: Location::Hangar(1) });
    }
    let commands = run(&mut line, &clock, 3.0, 20.0);
    assert_eq!(first_open(&commands).map(|open| (open.1, open.2)), Some((id, hangar(1))));
    // The owner gets out and leaves the game; their friend is still aboard when the hangar closes.
    line.handle(clock.at(20.0), Event::Exit { player: 1, carriage: id });
    line.handle(clock.at(21.0), Event::HangarClosed { owner: 1 });
    assert_eq!(line.carriage(id).unwrap().queue, vec![LOBBY]);
    let commands = run(&mut line, &clock, 21.0, 45.0);
    let (seconds, carriage, gateway) = first_open(&commands).expect("the doors open at the lobby");
    assert_eq!((carriage, gateway.stop), (id, LOBBY));
    assert!(seconds >= 37.0, "a full ride from the closing: {seconds}");
}

#[test]
fn riders_bound_for_a_hangar_that_closes_are_taken_to_the_lobby() {
    let (mut line, clock) = setup(3, Settings::default());
    let id = boarded(&mut line, &clock, 0, &[5]);
    line.handle(clock.at(2.0), Event::Pick { player: 5, stop: Stop::Hangar(1) });
    run(&mut line, &clock, 2.0, 9.0);
    assert_eq!(line.carriage(id).unwrap().state, CarriageState::Travelling);
    line.handle(clock.at(10.0), Event::HangarClosed { owner: 1 });
    assert_eq!(line.carriage(id).unwrap().queue, vec![LOBBY]);
    let commands = run(&mut line, &clock, 10.0, 30.0);
    let (seconds, carriage, gateway) = first_open(&commands).expect("the doors open at the lobby");
    assert_eq!((carriage, gateway.stop), (id, LOBBY));
    assert!(seconds >= 26.0, "a full ride from the closing: {seconds}");
}

#[test]
fn a_carriage_whose_riders_picked_nothing_gives_up_the_gateway() {
    let (mut line, clock) = setup(3, Settings::default());
    let lingering = boarded(&mut line, &clock, 0, &[1]);
    line.handle(clock.at(2.0), Event::Pick { player: 1, stop: Stop::Hangar(1) });
    line.handle(clock.at(2.0), Event::Ready { player: 1, location: Location::Hangar(1) });
    run(&mut line, &clock, 2.0, 30.0);
    assert_eq!(line.carriage(lingering).unwrap().at, Some(hangar(1)));
    // Rider 1 stays aboard; the doors closed. Another carriage brings rider 2 to the hangar's one gateway.
    let other = boarded(&mut line, &clock, 1, &[2]);
    line.handle(clock.at(31.0), Event::Pick { player: 2, stop: Stop::Hangar(1) });
    line.handle(clock.at(31.0), Event::Ready { player: 2, location: Location::Hangar(1) });
    let commands = run(&mut line, &clock, 31.0, 60.0);
    assert_eq!(first_open(&commands).map(|open| (open.1, open.2)), Some((other, hangar(1))));
    let waiting = line.carriage(lingering).unwrap();
    assert_eq!((waiting.state, waiting.riders), (CarriageState::WaitingInLimbo, vec![1]));
    // Rider 1 picks the hangar again and gets back once it is free.
    line.handle(clock.at(60.0), Event::Exit { player: 2, carriage: other });
    line.handle(clock.at(60.0), Event::Pick { player: 1, stop: Stop::Hangar(1) });
    let commands = run(&mut line, &clock, 60.0, 90.0);
    assert_eq!(first_open(&commands).map(|open| (open.1, open.2)), Some((lingering, hangar(1))));
}

#[test]
fn a_call_reopens_a_carriage_at_the_gateway() {
    let (mut line, clock) = setup(3, Settings::default());
    let id = boarded(&mut line, &clock, 0, &[1]);
    run(&mut line, &clock, 1.0, 10.0);
    assert!(!line.carriage(id).unwrap().doors_open);
    let commands = line.handle(clock.at(10.0), Event::Call { player: 2, gateway: lobby(0) });
    assert_eq!(opened(&commands), [(id, lobby(0))]);
    assert!(created(&commands).is_empty() && line.calls().is_empty());
    let commands = line.handle(clock.at(11.0), Event::OpenDoors { player: 1 });
    assert!(opened(&commands).is_empty(), "already open");
}

#[test]
fn the_line_of_orisons_hangar_elevator() {
    let tables = crate::transit::tables();
    let manager = tables.managers().iter().find(|manager| manager.name == "TransitManager_Spaceport_to_Hangars").unwrap();
    let spec = LineSpec::from_manager(manager);
    assert_eq!((spec.system, spec.fleet, spec.destinations.len(), spec.persistent), (System::Transit, 8, 7, 1));
    assert!(spec.destinations[0].gateways.is_empty(), "the Javelin tour is disabled");
    assert_eq!(spec.destinations[1].gateways, (2..12).collect::<Vec<_>>());
    assert_eq!(spec.destinations[6].gateways, [20, 21]);
    let settings = Settings::default().with_manager(manager);
    assert_eq!((settings.door_wait, settings.door_close), (Duration::from_secs(5), Duration::from_millis(2500)));
    let line = Line::new(spec, settings);
    assert!(line.gateways(Stop::Destination(0)).is_empty());
    assert_eq!(line.location(Stop::Destination(1)), Location::Persistent(1));
}

#[test]
fn a_transport_line_uses_its_fixed_slots() {
    let mut line = Line::new(LineSpec { system: System::Transport, ..spec(2) }, Settings::default());
    let clock = Clock(Instant::now());
    for gateway in 0..3 {
        line.handle(clock.at(0.0), Event::Call { player: gateway as u64 + 1, gateway: lobby(gateway) });
    }
    let ids: Vec<CarriageId> = line.carriages().iter().map(|carriage| carriage.id).collect();
    assert_eq!(ids, [CarriageId(0), CarriageId(1)]);
    assert_eq!(line.calls(), vec![lobby(2)]);
}

// Properties, over random sessions.

/// xorshift64*: deterministic and enough for picking events.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> Option<T> {
        (!items.is_empty()).then(|| items[self.below(items.len())])
    }
}

const PLAYERS: u64 = 8;
const OWNERS: u64 = 3;

/// The clients and the server around a line: who is aboard what, what each client has loaded, and the answers still to
/// come. Every command is checked against it.
struct Session {
    line: Line,
    clock: Clock,
    now: f64,
    rng: Rng,
    riders: BTreeMap<PlayerId, CarriageId>,
    ready: BTreeMap<PlayerId, BTreeSet<Location>>,
    /// Loads a client will answer, at that time.
    answers: Vec<(f64, PlayerId, Location)>,
    /// Whether clients may get stuck on a load (until it is sent again).
    stuck: bool,
    /// What has been checked, so a test can tell the session reached the cases.
    opens: usize,
    splits: usize,
    picks: usize,
}

impl Session {
    fn new(seed: u64, fleet: usize, settings: Settings) -> Self {
        let mut session = Self {
            line: Line::new(spec(fleet), settings),
            clock: Clock(Instant::now()),
            now: 0.0,
            rng: Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1),
            riders: BTreeMap::new(),
            ready: BTreeMap::new(),
            answers: Vec::new(),
            stuck: true,
            opens: 0,
            splits: 0,
            picks: 0,
        };
        for owner in 1..=OWNERS {
            session.send(Event::HangarOpened { owner, gateways: 1 + (owner as usize % 2) });
        }
        session
    }

    fn send(&mut self, event: Event) {
        match event {
            Event::Board { player, carriage } if !self.riders.contains_key(&player) => {
                if let Some(at) = self.line.carriage(carriage).and_then(|info| info.at) {
                    self.riders.insert(player, carriage);
                    self.ready.insert(player, BTreeSet::from([self.line.location(at.stop)]));
                }
            }
            Event::Exit { player, carriage } if self.riders.get(&player) == Some(&carriage) => self.forget(player),
            Event::Disconnected { player } => self.forget(player),
            Event::Ready { player, location } => {
                if self.riders.contains_key(&player) {
                    self.ready.get_mut(&player).unwrap().insert(location);
                }
            }
            Event::Pick { .. } => self.picks += 1,
            _ => {}
        }
        let idle_before: BTreeSet<CarriageId> =
            self.line.carriages().iter().filter(|carriage| carriage.state == CarriageState::Idle).map(|carriage| carriage.id).collect();
        let before: BTreeMap<CarriageId, Vec<PlayerId>> = self.line.carriages().into_iter().map(|carriage| (carriage.id, carriage.riders)).collect();
        let commands = self.line.handle(self.clock.at(self.now), event);
        self.check(&commands, &idle_before, &before);
    }

    fn tick(&mut self) {
        let idle_before: BTreeSet<CarriageId> =
            self.line.carriages().iter().filter(|carriage| carriage.state == CarriageState::Idle).map(|carriage| carriage.id).collect();
        let before: BTreeMap<CarriageId, Vec<PlayerId>> = self.line.carriages().into_iter().map(|carriage| (carriage.id, carriage.riders)).collect();
        let commands = self.line.tick(self.clock.at(self.now));
        self.check(&commands, &idle_before, &before);
    }

    fn forget(&mut self, player: PlayerId) {
        self.riders.remove(&player);
        self.ready.remove(&player);
        self.answers.retain(|(_, who, _)| *who != player);
    }

    fn check(&mut self, commands: &[Command], idle_before: &BTreeSet<CarriageId>, before: &BTreeMap<CarriageId, Vec<PlayerId>>) {
        let fleet = self.line.spec().fleet;
        let mut moves: Vec<(PlayerId, CarriageId, CarriageId)> = Vec::new();
        let mut creates = false;
        for command in commands {
            match *command {
                Command::Create { carriage, .. } => {
                    assert!(carriage.0 < fleet);
                    creates = true;
                }
                Command::Remove { carriage } => {
                    assert!(!self.riders.values().any(|aboard| *aboard == carriage), "a carriage with riders is removed");
                    assert!(before.get(&carriage).is_none_or(Vec::is_empty), "a carriage with riders is removed");
                }
                Command::MoveRider { player, from, to } => {
                    assert_eq!(self.riders.get(&player), Some(&from));
                    self.riders.insert(player, to);
                    moves.push((player, from, to));
                }
                Command::OpenDoors { carriage, gateway } => {
                    let location = self.line.location(gateway.stop);
                    for (player, aboard) in &self.riders {
                        if *aboard == carriage {
                            assert!(self.ready[player].contains(&location), "doors open at {gateway:?} not loaded by rider {player}");
                        }
                    }
                    self.opens += 1;
                }
                Command::Load { player, location, .. } => {
                    assert!(self.riders.contains_key(&player));
                    if !(self.stuck && self.rng.chance(20)) {
                        let delay = self.rng.below(250) as f64 / 10.0;
                        self.answers.push((self.now + delay, player, location));
                    }
                }
                Command::Unload { player, location } => {
                    if let Some(ready) = self.ready.get_mut(&player) {
                        ready.remove(&location);
                    }
                    self.answers.retain(|(_, who, what)| (*who, *what) != (player, location));
                }
                Command::Save { player, .. } => assert!(!self.riders.contains_key(&player)),
                Command::State { .. } | Command::CloseDoors { .. } => {}
            }
        }
        // A split moves at least one rider into one carriage and leaves at least one behind.
        if let Some((_, from, to)) = moves.first().copied() {
            assert!(moves.iter().all(|(_, f, t)| (*f, *t) == (from, to)), "one split at a time: {moves:?}");
            assert!(!self.line.carriage(from).unwrap().riders.is_empty() || self.line.carriage(from).is_none());
            assert!(before[&from].len() > moves.len(), "a split never takes every rider");
            self.splits += 1;
        }
        // The line and the session agree on who is aboard what.
        let carriages = self.line.carriages();
        for player in 1..=PLAYERS {
            assert_eq!(self.line.rider(player).map(|(carriage, _)| carriage), self.riders.get(&player).copied(), "rider {player}");
        }
        // Fewest carriages: within the fleet; none in limbo with nothing to do; an idle one while a call is not served
        // only if it is on its way; a new one only when none was idle.
        assert!(carriages.len() <= fleet);
        for carriage in &carriages {
            if matches!(carriage.state, CarriageState::Travelling | CarriageState::WaitingInLimbo) {
                assert!(!carriage.riders.is_empty() || carriage.call.is_some(), "{carriage:?}");
            }
            if carriage.state == CarriageState::Idle {
                assert!(carriage.riders.is_empty() && carriage.queue.is_empty() && carriage.call.is_none());
            }
            let mut stops = carriage.queue.clone();
            stops.sort();
            stops.dedup();
            assert_eq!(stops.len(), carriage.queue.len(), "a stop queued twice: {carriage:?}");
            for player in &carriage.riders {
                if let Some((_, Some(stop))) = self.line.rider(*player) {
                    assert!(carriage.queue.contains(&stop), "rider {player}'s pick {stop:?} is not queued: {carriage:?}");
                }
            }
        }
        if carriages.iter().any(|carriage| carriage.state == CarriageState::Idle) {
            for call in self.line.calls() {
                assert!(carriages.iter().any(|carriage| carriage.call == Some(call) || carriage.target == Some(call)), "call {call:?} unserved");
            }
        }
        if creates {
            for id in idle_before {
                assert!(self.line.carriage(*id).is_none_or(|carriage| carriage.state != CarriageState::Idle), "created while {id:?} was idle");
            }
        }
    }

    /// Answers the loads that are due.
    fn answer(&mut self) {
        let now = self.now;
        let due: Vec<(PlayerId, Location)> = self.answers.iter().filter(|(at, _, _)| *at <= now).map(|(_, player, location)| (*player, *location)).collect();
        self.answers.retain(|(at, _, _)| *at > now);
        for (player, location) in due {
            self.send(Event::Ready { player, location });
        }
    }

    fn random_event(&mut self) {
        let carriages = self.line.carriages();
        let players: Vec<PlayerId> = (1..=PLAYERS).collect();
        let outside: Vec<PlayerId> = players.iter().copied().filter(|player| !self.riders.contains_key(player)).collect();
        let aboard: Vec<PlayerId> = self.riders.keys().copied().collect();
        let roll = self.rng.below(100);
        let event = match roll {
            0..=14 => {
                let stop = if self.rng.chance(50) { LOBBY } else { self.random_stop() };
                let gateways = self.line.gateways(stop);
                match (self.rng.pick(&outside), self.rng.pick(&gateways)) {
                    (Some(player), Some(gateway)) => Some(Event::Call { player, gateway }),
                    _ => None,
                }
            }
            15..=34 => {
                let open: Vec<CarriageId> = carriages.iter().filter(|carriage| carriage.doors_open).map(|carriage| carriage.id).collect();
                match (self.rng.pick(&outside), self.rng.pick(&open)) {
                    (Some(player), Some(carriage)) => Some(Event::Board { player, carriage }),
                    _ => None,
                }
            }
            35..=44 => {
                let leaving: Vec<PlayerId> =
                    aboard.iter().copied().filter(|player| self.line.carriage(self.riders[player]).is_some_and(|carriage| carriage.doors_open)).collect();
                self.rng.pick(&leaving).map(|player| Event::Exit { player, carriage: self.riders[&player] })
            }
            45..=66 => {
                let stop = self.random_stop();
                self.rng.pick(&aboard).map(|player| Event::Pick { player, stop })
            }
            67..=69 => self.rng.pick(&aboard).map(|player| Event::OpenDoors { player }),
            70..=71 => self.rng.pick(&aboard).map(|player| Event::HoldDoors { player }),
            72..=74 => self.rng.pick(&players).map(|player| Event::Disconnected { player }),
            75 => {
                let owner = 1 + self.rng.below(OWNERS as usize) as u64;
                Some(if self.rng.chance(50) { Event::HangarClosed { owner } } else { Event::HangarOpened { owner, gateways: 1 } })
            }
            _ => None,
        };
        match event {
            Some(event) => self.send(event),
            None => self.tick(),
        }
    }

    fn random_stop(&mut self) -> Stop {
        match self.rng.below(5) {
            0 => LOBBY,
            1 => PORT,
            n => Stop::Hangar(n as u64 - 1),
        }
    }

    /// Time passes: up to 2 s per step.
    fn wait(&mut self, tenths: usize) {
        self.now += self.rng.below(tenths + 1) as f64 / 10.0;
    }

    /// No more events but the clients' answers, all of them, for `seconds`.
    fn drain(&mut self, seconds: f64) {
        self.stuck = false;
        for answer in &mut self.answers {
            answer.0 = answer.0.min(self.now);
        }
        let end = self.now + seconds;
        while self.now < end {
            self.now += 0.5;
            self.answer();
            self.tick();
        }
    }
}

fn session(seed: u64, fleet: usize, settings: Settings, steps: usize) -> Session {
    let mut session = Session::new(seed, fleet, settings);
    for _ in 0..steps {
        session.wait(20);
        session.answer();
        session.random_event();
    }
    session
}

/// Rule 3, rule 2, the fleet, splits and the agreement between line and clients hold at every step (checked in
/// `Session::check`), and once events stop, every pick is served or dropped and every carriage without riders goes.
fn properties(fleet: usize, settings: Settings) -> (usize, usize, usize) {
    let (mut opens, mut splits, mut picks) = (0, 0, 0);
    for seed in 0..150 {
        let mut session = session(seed, fleet, settings.clone(), 400);
        session.drain(400.0);
        for carriage in session.line.carriages() {
            assert!(carriage.queue.is_empty(), "seed {seed}: a pick never served: {carriage:?}");
        }
        for player in 1..=PLAYERS {
            assert!(session.line.rider(player).is_none_or(|(_, pick)| pick.is_none()), "seed {seed}: rider {player} still waits");
        }
        for player in 1..=PLAYERS {
            session.send(Event::Disconnected { player });
        }
        session.drain(session.line.settings().idle_removal.as_secs_f64() + 20.0);
        assert!(session.line.carriages().is_empty(), "seed {seed}: carriages left without riders: {:?}", session.line.carriages());
        opens += session.opens;
        splits += session.splits;
        picks += session.picks;
    }
    (opens, splits, picks)
}

#[test]
fn the_rules_hold_over_random_sessions() {
    let (opens, splits, picks) = properties(4, Settings::default());
    assert!(opens > 1000 && splits > 20 && picks > 1000, "the sessions reach the cases: {opens} opens, {splits} splits, {picks} picks");
}

#[test]
fn the_rules_hold_with_a_small_fleet_and_a_preload_limit() {
    properties(2, Settings { preload_limit: Some(0), ..Settings::default() });
}

#[test]
fn the_rules_hold_without_splits() {
    let (_, splits, _) = properties(3, Settings { split: false, ..Settings::default() });
    assert_eq!(splits, 0);
}

#[test]
fn the_same_events_give_the_same_commands() {
    let script = |line: &mut Line, clock: &Clock| -> Vec<Command> {
        let mut out = Vec::new();
        out.extend(line.handle(clock.at(0.0), Event::HangarOpened { owner: 1, gateways: 1 }));
        out.extend(line.handle(clock.at(0.0), Event::Call { player: 1, gateway: lobby(0) }));
        out.extend(line.handle(clock.at(1.0), Event::Board { player: 1, carriage: CarriageId(0) }));
        out.extend(line.handle(clock.at(1.0), Event::Board { player: 2, carriage: CarriageId(0) }));
        out.extend(line.handle(clock.at(2.0), Event::Pick { player: 2, stop: Stop::Hangar(1) }));
        out.extend(line.handle(clock.at(4.0), Event::Ready { player: 2, location: Location::Hangar(1) }));
        for tenth in 40..800 {
            out.extend(line.tick(clock.at(tenth as f64 / 10.0)));
        }
        out
    };
    let clock = Clock(Instant::now());
    let first = script(&mut Line::new(spec(3), Settings::default()), &clock);
    let second = script(&mut Line::new(spec(3), Settings::default()), &clock);
    assert_eq!(first, second);
    assert!(first.iter().any(|command| matches!(command, Command::MoveRider { .. })));
}
