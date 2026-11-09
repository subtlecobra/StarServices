//! The replicant's calls to the DGSs of its shard. Everything here runs beside the packet loop, which must never wait: a
//! client that gets no pong drops the channel. Answers reach the loop as `Event`s, and a peer stays in its stage until the
//! answer it needs is there.
//!
//! A DGS builds every entity. If it cannot be reached a join holds where it is and asks again, for as long as the
//! connection lasts, which the client lets run for 720 seconds. Nothing here makes up what a DGS did not send.

use std::{
    collections::HashMap,
    future::Future,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use sc_api::internal::{
    dgs_control_client::DgsControlClient, hub_internal_client::HubInternalClient, session_update::Update, DisconnectReason, PlayerDisconnect, DgsStatusRequest, DgsStatusResponse, EntityBind, EntityMethodCall,
    EntityMethodRequest, InputFrame, JoinPlayerRequest, LeavePlayerRequest, SessionUpdate, SessionUpdatesRequest,
    LocateHomeRequest, LocateHomeResponse, PlayerInputRequest, PlayerMovement, ShardEntitiesRequest, ShardEntitiesResponse,
};
use tokio::{
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    time::sleep,
};
use tonic::{
    transport::{Channel, Endpoint},
    Code, Request, Status,
};
use tracing::{debug, info, warn};

use crate::dgs::Directory;

/// How long a held join waits before asking again.
const RETRY_PERIOD: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Finds a connection to a pod that went away without saying so.
const KEEP_ALIVE_PERIOD: Duration = Duration::from_secs(5);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(3);
/// A leave nobody hears only leaves a record behind, so it is not retried for long.
const LEAVE_ATTEMPTS: u32 = 5;
/// The Hub is told once as often. A report it never hears is the Hub's own to find: its connection to the client ends too.
const HUB_ATTEMPTS: u32 = 5;

/// What every client gets, in the order it goes on the wire.
#[derive(Clone, Debug, PartialEq)]
pub struct World {
    /// The game rules and the game mode.
    pub universe: Vec<EntityBind>,
    /// Every solar system, then every quantum grid, as a planet needs its system's grid bound first.
    pub solar_systems: Vec<EntityBind>,
}

/// A player's entities, from the DGS that holds them.
#[derive(Clone, Debug, PartialEq)]
pub struct Joined {
    pub dgs: String,
    /// What the overlay names `dgs` as.
    pub server_name: String,
    pub planet: Option<EntityBind>,
    pub hangar: Option<EntityBind>,
    /// The containers inside the hangar, bound in their own batch after it.
    pub hangar_content: Vec<EntityBind>,
    pub player: EntityBind,
    /// What the player wears, bound after it in its batch.
    pub player_items: Vec<EntityBind>,
    pub status: DgsStatusResponse,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlayerEntities {
    /// The governing DGS has not answered.
    Waiting,
    /// This connection has no character, as a session that only checks the shard. It goes no further than the
    /// solar systems.
    NoCharacter,
    Joined(Box<Joined>),
}

/// Told to the packet loop, for the connection `from`. `session` tells it from an earlier one at that address.
#[derive(Debug)]
pub enum Event {
    World { from: SocketAddr, session: u64, world: World },
    Player { from: SocketAddr, session: u64, player: PlayerEntities },
    /// `None` when the DGS does not answer.
    Status { dgs: String, status: Option<DgsStatusResponse> },
    /// Where the DGS has the player after its input. `None` when it did not answer.
    Movement { from: SocketAddr, session: u64, movement: Option<PlayerMovement> },
    /// What the DGS sends back to a client for one of its remote method calls.
    Calls { from: SocketAddr, session: u64, calls: Vec<EntityMethodCall> },
    /// What the DGS has for the client of its own accord, such as a carriage of its hangar's elevator.
    Update { from: SocketAddr, session: u64, update: Update },
}

enum Command {
    /// The DGS restarted, and has forgotten this player.
    Rejoin,
}

/// A connection's hold on its DGS. Dropping this is what ends it: whichever way a peer goes, its entities are freed.
pub struct Session {
    commands: UnboundedSender<Command>,
    ending: Arc<Mutex<End>>,
}

/// Why a connection ended, as its peer says when it knows. A peer dropped without a word timed out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct End {
    pub reason: DisconnectReason,
    /// The client's own cause code from its disconnect notice (30016 for a quit), 0 for none.
    pub client_code: u32,
}

impl Default for End {
    fn default() -> Self {
        Self { reason: DisconnectReason::Timeout, client_code: 0 }
    }
}

impl Session {
    /// Says why the connection is about to end. The last word counts, and the end itself is the drop of the session.
    pub fn end(&self, reason: DisconnectReason, client_code: u32) {
        *self.ending.lock().unwrap() = End { reason, client_code };
    }

    pub fn rejoin(&self) {
        let _ = self.commands.send(Command::Rejoin);
    }
}

/// What a connection asks of the DGS, from its Hub session.
#[derive(Clone, Copy, Debug)]
pub struct Identity {
    /// `None` for a session with no account.
    pub account: Option<u64>,
    pub location: Option<u32>,
    /// The player's GEID, as the Hub has it, which it needs to free the player's shard slot.
    pub geid: Option<u64>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn reason(dgs: &str, status: &Status) -> String {
    format!("{dgs}: {} {}", status.code(), status.message())
}

pub struct Link {
    directory: Directory,
    /// This process, so that a DGS can tell a restarted replicant.
    replicant: u64,
    retry: Duration,
    channels: Mutex<HashMap<String, Channel>>,
    events: UnboundedSender<Event>,
    /// The Hub, which owns sessions and the shard slot, and is told when a connection ends. With none nobody is.
    hub: Option<HubInternalClient<Channel>>,
    shard_id: String,
}

impl Link {
    pub fn new(directory: Directory, events: UnboundedSender<Event>) -> Self {
        let replicant = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
        Self { directory, replicant, retry: RETRY_PERIOD, channels: Mutex::default(), events, hub: None, shard_id: String::new() }
    }

    /// Tells the Hub at `address` when a connection of this shard ends (`HubInternal.ReportDisconnect`).
    pub fn reporting_to(mut self, address: &str, shard_id: &str) -> Result<Self, String> {
        let endpoint = Endpoint::from_shared(address.to_string()).map_err(|e| format!("--hub {address}: {e}"))?;
        self.hub = Some(HubInternalClient::new(endpoint.connect_timeout(CONNECT_TIMEOUT).timeout(CALL_TIMEOUT).connect_lazy()));
        self.shard_id = shard_id.to_string();
        Ok(self)
    }

    #[cfg(test)]
    pub fn retrying_every(mut self, retry: Duration) -> Self {
        self.retry = retry;
        self
    }

    fn client(&self, dgs: &str) -> Result<DgsControlClient<Channel>, String> {
        let mut channels = self.channels.lock().unwrap();
        if let Some(channel) = channels.get(dgs) {
            return Ok(DgsControlClient::new(channel.clone()));
        }
        let address = self.directory.endpoint(dgs).ok_or_else(|| format!("no address for DGS {dgs}"))?;
        let endpoint = Endpoint::from_shared(address.to_string()).map_err(|error| format!("{dgs}: bad address {address}: {error}"))?;
        let channel = endpoint
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(CALL_TIMEOUT)
            .http2_keep_alive_interval(KEEP_ALIVE_PERIOD)
            .keep_alive_timeout(KEEP_ALIVE_TIMEOUT)
            .keep_alive_while_idle(true)
            .connect_lazy();
        channels.insert(dgs.to_string(), channel.clone());
        Ok(DgsControlClient::new(channel))
    }

    /// A channel that failed may hold a dead connection, so the next call starts a new one.
    fn failed(&self, dgs: &str, status: &Status) -> String {
        if matches!(status.code(), Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled | Code::Unknown) {
            self.channels.lock().unwrap().remove(dgs);
        }
        reason(dgs, status)
    }

    /// Asks every system's DGS for what it owns. All of them, or the client would get a universe with a system missing.
    async fn world(&self) -> Result<World, String> {
        let owners = self.directory.system_owners();
        if owners.is_empty() {
            return Err("no DGS has a system root".to_string());
        }
        let mut replies: Vec<ShardEntitiesResponse> = Vec::new();
        for dgs in owners {
            let reply = self.client(dgs)?.shard_entities(Request::new(ShardEntitiesRequest {})).await;
            replies.push(reply.map_err(|status| self.failed(dgs, &status))?.into_inner());
        }
        replies.sort_by_key(|reply| reply.order);
        let universe = replies.iter().map(|reply| &reply.universe).find(|universe| !universe.is_empty());
        let universe = universe.ok_or("no DGS sent the game rules")?.clone();
        let systems = replies.iter().map(|reply| reply.solar_system.clone());
        let grids = replies.iter().map(|reply| reply.quantum_grid.clone());
        let solar_systems = systems.chain(grids).collect::<Option<Vec<_>>>().ok_or("a DGS with a system root sent no system")?;
        Ok(World { universe, solar_systems })
    }

    /// Any DGS can say what governs a home, so one that is down only matters if it is the one that does.
    async fn locate(&self, location: Option<u32>) -> Result<LocateHomeResponse, String> {
        let mut last = "no DGS to ask".to_string();
        for dgs in self.directory.names() {
            match self.client(dgs)?.locate_home(Request::new(LocateHomeRequest { location_id: location })).await {
                Ok(located) => return Ok(located.into_inner()),
                Err(status) => last = self.failed(dgs, &status),
            }
        }
        Err(last)
    }

    async fn join(&self, session: u64, account: u64, location: Option<u32>, asked: &Mutex<Option<String>>) -> Result<PlayerEntities, String> {
        let located = self.locate(location).await?;
        let planet = (!located.planet.is_empty()).then_some(located.planet.as_str());
        let Some(dgs) = self.directory.governing(&located.system, planet) else {
            return Err(format!("no DGS governs {} {}", located.system, located.planet));
        };
        // Set before the call: a reply that never arrives may still have been recorded.
        *asked.lock().unwrap() = Some(dgs.to_string());
        let request = JoinPlayerRequest { replicant: self.replicant, session, account_id: account, location_id: location };
        let reply = match self.client(dgs)?.join_player(Request::new(request)).await {
            Ok(reply) => reply.into_inner(),
            Err(status) if status.code() == Code::InvalidArgument => {
                info!(account, %status, "no character for this session");
                return Ok(PlayerEntities::NoCharacter);
            }
            Err(status) => return Err(self.failed(dgs, &status)),
        };
        let player = reply.player.ok_or_else(|| format!("{dgs} sent no player"))?;
        let status = reply.status.ok_or_else(|| format!("{dgs} sent no status"))?;
        let (planet, hangar, hangar_content, player_items) = (reply.planet, reply.hangar, reply.hangar_content, reply.player_items);
        let server_name = self.directory.server_name(dgs);
        let joined = Joined { dgs: dgs.to_string(), server_name, planet, hangar, hangar_content, player, player_items, status };
        Ok(PlayerEntities::Joined(Box::new(joined)))
    }

    /// Keeps calling until an answer comes. Cancelled by the end of the connection.
    async fn persist<T, F: Future<Output = Result<T, String>>>(&self, from: SocketAddr, what: &str, mut attempt: impl FnMut() -> F) -> T {
        loop {
            match attempt().await {
                Ok(answer) => return answer,
                Err(reason) => warn!(%from, what, %reason, "join held for a DGS, asking again"),
            }
            sleep(self.retry).await;
        }
    }

    async fn leave(&self, dgs: &str, session: u64, end: End, at_ms: u64) {
        for _ in 0..LEAVE_ATTEMPTS {
            let request = LeavePlayerRequest { replicant: self.replicant, session, reason: end.reason as i32, client_code: end.client_code, at_ms };
            let result = match self.client(dgs) {
                Ok(mut client) => client.leave_player(Request::new(request)).await.map_err(|status| self.failed(dgs, &status)),
                Err(reason) => Err(reason),
            };
            match result {
                Ok(reply) => {
                    info!(dgs, session, released = reply.into_inner().released, "player's entities freed");
                    return;
                }
                Err(reason) => debug!(dgs, session, %reason, "leave not heard"),
            }
            sleep(self.retry).await;
        }
        warn!(dgs, session, "leave not heard, the DGS keeps this player's record until it is replaced");
    }

    /// The event the Hub is told, and the DGS's leave carries the same key. `None` for a connection that is no one's, or that
    /// a newer connection of the same player took over: the player has not left.
    fn disconnect(&self, session: u64, identity: Identity, end: End, connected_at_ms: u64, at_ms: u64) -> Option<PlayerDisconnect> {
        let (account, geid) = (identity.account?, identity.geid?);
        (end.reason != DisconnectReason::Replaced).then(|| PlayerDisconnect {
            event_id: format!("{}:{session}", self.replicant),
            reason: end.reason as i32,
            account_id: u32::try_from(account).unwrap_or_default(),
            geid,
            shard_id: self.shard_id.clone(),
            client_code: end.client_code,
            connected_at_ms,
            at_ms,
        })
    }

    /// Tells the Hub, on a task of its own: the end of a connection is never held up by a Hub that is slow or away.
    fn report(self: &Arc<Self>, event: PlayerDisconnect) {
        let Some(hub) = self.hub.clone() else { return };
        let retry = self.retry;
        tokio::spawn(async move {
            for attempt in 1..=HUB_ATTEMPTS {
                match hub.clone().report_disconnect(Request::new(event.clone())).await {
                    Ok(reply) => {
                        let reply = reply.into_inner();
                        info!(event = %event.event_id, recorded = reply.recorded, slot_freed = reply.slot_freed, reason = ?event.reason(), "the Hub knows the player left");
                        return;
                    }
                    Err(status) if attempt < HUB_ATTEMPTS => debug!(event = %event.event_id, %status, "the Hub did not hear the disconnect"),
                    Err(status) => warn!(event = %event.event_id, %status, "the Hub did not hear the disconnect: it ends the player's session with its own connection"),
                }
                sleep(retry).await;
            }
        });
    }

    /// One connection, from asking for its entities until its peer is dropped, when it frees them.
    async fn run(self: Arc<Self>, from: SocketAddr, session: u64, identity: Identity, ending: Arc<Mutex<End>>, mut commands: UnboundedReceiver<Command>) {
        let connected_at_ms = now_ms();
        let asked = Mutex::new(None);
        let joining = async {
            let world = self.persist(from, "solar systems", || self.world()).await;
            let _ = self.events.send(Event::World { from, session, world });
            let player = match identity.account {
                Some(account) => self.persist(from, "player", || self.join(session, account, identity.location, &asked)).await,
                None => PlayerEntities::NoCharacter,
            };
            let _ = self.events.send(Event::Player { from, session, player });
        };
        tokio::pin!(joining);
        let connected = loop {
            tokio::select! {
                () = &mut joining => break true,
                command = commands.recv() => if command.is_none() { break false },
            }
        };
        if connected {
            // The DGS's updates for this session, until the connection ends. A stream that ends, as when the DGS restarts,
            // is opened again; a rejoin opens it on the DGS that took the player back.
            let mut updates: Option<tonic::Streaming<SessionUpdate>> = None;
            loop {
                if updates.is_none() {
                    let dgs = asked.lock().unwrap().clone();
                    if let Some(dgs) = dgs {
                        updates = self.subscribe(&dgs, session).await;
                    }
                }
                let next = async {
                    match updates.as_mut() {
                        Some(stream) => stream.message().await,
                        None => {
                            sleep(self.retry).await;
                            Ok(None)
                        }
                    }
                };
                tokio::select! {
                    command = commands.recv() => match command {
                        Some(Command::Rejoin) => {
                            updates = None;
                            let Some(account) = identity.account else { continue };
                            while self.join(session, account, identity.location, &asked).await.is_err() && !commands.is_closed() {
                                sleep(self.retry).await;
                            }
                        }
                        None => break,
                    },
                    message = next => match message {
                        Ok(Some(SessionUpdate { update: Some(update) })) => {
                            let _ = self.events.send(Event::Update { from, session, update });
                        }
                        Ok(Some(_)) => {}
                        Ok(None) | Err(_) => {
                            if updates.take().is_some() {
                                debug!(%from, session, "the DGS's updates ended, subscribing again");
                                sleep(self.retry).await;
                            }
                        }
                    },
                }
            }
        }
        let end = *ending.lock().unwrap();
        let at_ms = now_ms();
        info!(%from, session, reason = ?end.reason, client_code = end.client_code, "connection ended");
        if let Some(event) = self.disconnect(session, identity, end, connected_at_ms, at_ms) {
            self.report(event);
        }
        let dgs = asked.lock().unwrap().clone();
        if let Some(dgs) = dgs {
            self.leave(&dgs, session, end, at_ms).await;
        }
    }

    /// Opens a session's stream of updates on `dgs`. `None` when the DGS does not answer.
    async fn subscribe(&self, dgs: &str, session: u64) -> Option<tonic::Streaming<SessionUpdate>> {
        let mut client = self.client(dgs).inspect_err(|reason| debug!(%reason, "no updates")).ok()?;
        let request = SessionUpdatesRequest { replicant: self.replicant, session };
        match client.session_updates(Request::new(request)).await {
            Ok(response) => Some(response.into_inner()),
            Err(status) => {
                debug!(reason = %reason(dgs, &status), "no updates");
                None
            }
        }
    }

    /// Starts a connection's calls. Needs a runtime, so the packet path does this and not `Peer::new`.
    pub fn open(self: &Arc<Self>, from: SocketAddr, session: u64, identity: Identity) -> Session {
        let (commands, receiver) = unbounded_channel();
        let ending = Arc::new(Mutex::new(End::default()));
        tokio::spawn(self.clone().run(from, session, identity, ending.clone(), receiver));
        Session { commands, ending }
    }

    /// Hands a player's input frames to the DGS that holds it, and tells the loop where the player now is. One call per
    /// connection at a time, which the caller sees to, so frames arrive in order.
    pub fn input(self: &Arc<Self>, from: SocketAddr, session: u64, dgs: String, entity_id: u64, frames: Vec<InputFrame>) {
        let link = self.clone();
        tokio::spawn(async move {
            let request = PlayerInputRequest { replicant: link.replicant, session, entity_id, frames };
            let reply = match link.client(&dgs) {
                Ok(mut client) => client.player_input(Request::new(request)).await.map_err(|status| link.failed(&dgs, &status)),
                Err(reason) => Err(reason),
            };
            let movement = reply.map(|reply| reply.into_inner().movement).inspect_err(|reason| debug!(%reason, "input not taken")).ok().flatten();
            let _ = link.events.send(Event::Movement { from, session, movement });
        });
    }

    /// Hands a client's remote method call on an entity other than its player to the DGS that holds the player, and the
    /// calls it answers with back to the loop. A DGS that does not hold the entity, or does not answer, sends nothing.
    pub fn entity_method(self: &Arc<Self>, from: SocketAddr, session: u64, dgs: String, call: EntityMethodRequest) {
        let link = self.clone();
        tokio::spawn(async move {
            let (entity, method) = (call.entity_id, call.method);
            let request = EntityMethodRequest { replicant: link.replicant, session, ..call };
            let reply = match link.client(&dgs) {
                Ok(mut client) => client.entity_method(Request::new(request)).await.map_err(|status| format!("{} {}", status.code(), status.message())),
                Err(reason) => Err(reason),
            };
            match reply {
                Ok(reply) => {
                    let calls = reply.into_inner().to_caller;
                    info!(%from, entity, method, answers = calls.len(), "entity method taken by the DGS");
                    let _ = link.events.send(Event::Calls { from, session, calls });
                }
                Err(reason) => info!(%from, entity, method, %reason, "entity method not taken"),
            }
        });
    }

    /// Asks a DGS for its frames and population, for the overlay.
    pub fn poll(self: &Arc<Self>, dgs: String) {
        let link = self.clone();
        tokio::spawn(async move {
            let reply = match link.client(&dgs) {
                Ok(mut client) => client.status(Request::new(DgsStatusRequest {})).await.map_err(|status| link.failed(&dgs, &status)),
                Err(reason) => Err(reason),
            };
            let status = reply.map(tonic::Response::into_inner).inspect_err(|reason| warn!(%reason, "DGS not answering")).ok();
            let _ = link.events.send(Event::Status { dgs, status });
        });
    }
}

/// What the Hub is told when a connection ends, against a Hub that records the reports it gets.
#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use sc_api::internal::{
        hub_internal_server::{HubInternal, HubInternalServer},
        GetPlayerShardRequest, GetPlayerShardResponse, ReportDisconnectResponse, ResolveSessionRequest, ResolveSessionResponse,
    };
    use tokio::time::timeout;
    use tonic::transport::Server;

    use super::*;

    struct FakeHub {
        reports: UnboundedSender<PlayerDisconnect>,
        /// Calls refused before one is taken.
        refuse: AtomicU32,
    }

    #[tonic::async_trait]
    impl HubInternal for FakeHub {
        async fn get_player_shard(&self, _: Request<GetPlayerShardRequest>) -> Result<tonic::Response<GetPlayerShardResponse>, Status> {
            Err(Status::unimplemented(""))
        }
        async fn resolve_session(&self, _: Request<ResolveSessionRequest>) -> Result<tonic::Response<ResolveSessionResponse>, Status> {
            Err(Status::unimplemented(""))
        }
        async fn report_disconnect(&self, request: Request<PlayerDisconnect>) -> Result<tonic::Response<ReportDisconnectResponse>, Status> {
            if self.refuse.load(Ordering::SeqCst) > 0 {
                self.refuse.fetch_sub(1, Ordering::SeqCst);
                return Err(Status::unavailable("restarting"));
            }
            let _ = self.reports.send(request.into_inner());
            Ok(tonic::Response::new(ReportDisconnectResponse { recorded: true, slot_freed: true }))
        }
    }

    async fn link(refuse: u32) -> (Arc<Link>, tokio::sync::mpsc::UnboundedReceiver<PlayerDisconnect>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (reports, received) = unbounded_channel();
        let hub = HubInternalServer::new(FakeHub { reports, refuse: AtomicU32::new(refuse) });
        tokio::spawn(Server::builder().add_service(hub).serve_with_incoming(tonic::transport::server::TcpIncoming::from(listener)));
        let (events, _) = unbounded_channel();
        let directory = Directory::parse("shard-a", "pyro:pyro-1=pyrosystem", "").unwrap();
        let link = Link::new(directory, events).reporting_to(&format!("http://{address}"), "shard-a").unwrap().retrying_every(Duration::from_millis(5));
        (Arc::new(link), received)
    }

    fn player() -> Identity {
        Identity { account: Some(1_000_001), location: None, geid: Some(300_000_000_001) }
    }

    fn open(link: &Arc<Link>, session: u64, identity: Identity) -> Session {
        link.open("127.0.0.1:1".parse().unwrap(), session, identity)
    }

    #[tokio::test]
    async fn a_client_that_said_it_was_going_is_reported_clean_with_its_code() {
        let (link, mut reports) = link(0).await;
        let session = open(&link, 7, player());
        session.end(DisconnectReason::Clean, 30016);
        drop(session);
        let event = timeout(Duration::from_secs(5), reports.recv()).await.unwrap().unwrap();
        assert_eq!((event.reason(), event.client_code, event.shard_id.as_str()), (DisconnectReason::Clean, 30016, "shard-a"));
        assert_eq!((event.geid, event.account_id), (300_000_000_001, 1_000_001));
        assert_eq!(event.event_id, format!("{}:7", link.replicant));
        assert!(event.at_ms >= event.connected_at_ms && event.connected_at_ms > 0);
    }

    #[tokio::test]
    async fn a_connection_that_goes_silent_is_a_timeout() {
        let (link, mut reports) = link(0).await;
        drop(open(&link, 8, player()));
        let event = timeout(Duration::from_secs(5), reports.recv()).await.unwrap().unwrap();
        assert_eq!(event.reason(), DisconnectReason::Timeout);
    }

    #[tokio::test]
    async fn a_hub_that_is_away_for_a_moment_still_hears_it() {
        let (link, mut reports) = link(2).await;
        drop(open(&link, 9, player()));
        let event = timeout(Duration::from_secs(5), reports.recv()).await.unwrap().unwrap();
        assert_eq!(event.event_id, format!("{}:9", link.replicant));
    }

    #[tokio::test]
    async fn a_replaced_connection_and_one_with_no_player_report_nothing() {
        let (link, mut reports) = link(0).await;
        let replaced = open(&link, 10, player());
        replaced.end(DisconnectReason::Replaced, 0);
        drop(replaced);
        drop(open(&link, 11, Identity { account: None, location: None, geid: None }));
        drop(open(&link, 12, Identity { geid: None, ..player() }));
        assert!(timeout(Duration::from_millis(300), reports.recv()).await.is_err());
    }
}
