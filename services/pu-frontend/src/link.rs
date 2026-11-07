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

use sc_proto::internal::{
    dgs_control_client::DgsControlClient, DgsStatusRequest, DgsStatusResponse, EntityBind, InputFrame, JoinPlayerRequest, LeavePlayerRequest,
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
}

enum Command {
    /// The DGS restarted, and has forgotten this player.
    Rejoin,
}

/// A connection's hold on its DGS. Dropping this is what ends it: whichever way a peer goes, its entities are freed.
pub struct Session {
    commands: UnboundedSender<Command>,
}

impl Session {
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
}

impl Link {
    pub fn new(directory: Directory, events: UnboundedSender<Event>) -> Self {
        let replicant = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
        Self { directory, replicant, retry: RETRY_PERIOD, channels: Mutex::default(), events }
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

    async fn leave(&self, dgs: &str, session: u64) {
        for _ in 0..LEAVE_ATTEMPTS {
            let request = LeavePlayerRequest { replicant: self.replicant, session };
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

    /// One connection, from asking for its entities until its peer is dropped, when it frees them.
    async fn run(self: Arc<Self>, from: SocketAddr, session: u64, identity: Identity, mut commands: UnboundedReceiver<Command>) {
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
            while let Some(Command::Rejoin) = commands.recv().await {
                let Some(account) = identity.account else { continue };
                while self.join(session, account, identity.location, &asked).await.is_err() && !commands.is_closed() {
                    sleep(self.retry).await;
                }
            }
        }
        let dgs = asked.lock().unwrap().clone();
        if let Some(dgs) = dgs {
            self.leave(&dgs, session).await;
        }
    }

    /// Starts a connection's calls. Needs a runtime, so the packet path does this and not `Peer::new`.
    pub fn open(self: &Arc<Self>, from: SocketAddr, session: u64, identity: Identity) -> Session {
        let (commands, receiver) = unbounded_channel();
        tokio::spawn(self.clone().run(from, session, identity, receiver));
        Session { commands }
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
