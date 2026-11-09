//! A bot's session on a shard: log in at the Hub, connect, walk the join as the real client does, and from then on record
//! what the server sends. Everything is driven by the caller awaiting `run_until`; nothing runs in the background.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use crate::{
    hub::{Hub, Join, Login},
    nub::{Channel, Received},
    wire::{self, Bind, Reader},
    world::{Event, EventKind, World},
};

/// Where and as whom to join. Nothing is built in: every field comes from the command line or the environment.
#[derive(Clone, Debug, Default)]
pub struct Target {
    /// The Hub, `host:port`. With `login_data` it may come from its `star_network`.
    pub hub: Option<String>,
    /// `client.crt`, the certificate authority the Hub's certificate chains to.
    pub ca: Option<PathBuf>,
    /// The account token the Hub login needs.
    pub token: Option<String>,
    /// The location to go to; 0 for the Hub's default.
    pub location: u64,
    /// The shard's UDP address, when it is not the one the Hub names (a cluster's public address may not be reachable).
    pub shard: Option<String>,
    /// A session id to connect with, in place of a Hub login.
    pub session: Option<String>,
    pub join_timeout: Duration,
}

/// `eCVS_InGame`.
const STATE_IN_GAME: u8 = 17;
const KEEP_ALIVE_EVERY: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

pub struct Session {
    channel: Channel,
    /// Kept open: its end tells the Hub the player left.
    hub: Option<Hub>,
    pub login: Option<Login>,
    pub join: Option<Join>,
    pub world: World,
    pub events: Vec<Event>,
    started: Instant,
    last_ping: Instant,
    pub server_state: u8,
    pub client_state: u8,
    batch_ends: u32,
    /// The shard's id and name (message 5).
    pub shard: Option<(String, String)>,
    /// The map and game mode of `GameContextInfo`.
    pub context: Option<(String, String)>,
    /// The name of the DGS (`DebugAuthorityChangeInfo`).
    pub server_name: Option<String>,
    pub snapshot_updates: u64,
    /// Message ids seen, by count, for the report.
    pub message_counts: std::collections::BTreeMap<u32, u64>,
    pub pongs: Vec<(u64, u64)>,
    pub disconnect_acked: bool,
    pub disconnected: Option<(u32, String)>,
}

impl Session {
    /// Logs in, connects and walks the join to `eCVS_InGame`.
    pub async fn join(target: &Target) -> Result<Session, String> {
        let (mut hub, mut login, mut join, session_id) = (None, None, None, target.session.clone());
        let mut session_id = session_id;
        if session_id.is_none() {
            let address = target.hub.as_deref().ok_or("no Hub (--hub) and no --session to connect with")?;
            let ca = target.ca.as_deref().ok_or("no certificate authority (--ca) for the Hub")?;
            let token = target.token.as_deref().ok_or("no account token (--token or --login-data)")?;
            let mut connection = Hub::connect(address, ca).await?;
            let found = connection.login(token).await?;
            join = Some(connection.goto_location(target.location).await?);
            session_id = Some(found.session.clone());
            login = Some(found);
            hub = Some(connection);
        }
        let remote = match (&target.shard, &join) {
            (Some(shard), _) => shard.clone(),
            (None, Some(join)) => format!("{}:{}", join.address, join.port),
            (None, None) => return Err("no shard address (--shard) with a --session".into()),
        };
        let session_id = session_id.unwrap_or_default();
        let channel = Channel::connect(&remote, session_id.as_bytes(), target.join_timeout).await?;
        let mut session = Session {
            channel,
            hub,
            login,
            join,
            world: World::default(),
            events: Vec::new(),
            started: Instant::now(),
            last_ping: Instant::now(),
            server_state: 0,
            client_state: 0,
            batch_ends: 0,
            shard: None,
            context: None,
            server_name: None,
            snapshot_updates: 0,
            message_counts: Default::default(),
            pongs: Vec::new(),
            disconnect_acked: false,
            disconnected: None,
        };
        // The first data packet starts the conversation: the shard greets a client once it has heard from it.
        session.report(3).await?;
        if !session.run_until(target.join_timeout, |s| s.server_state >= STATE_IN_GAME).await? {
            return Err(format!("the join did not reach eCVS_InGame in {:?} (the server is at state {}, the bot at {})", target.join_timeout, session.server_state, session.client_state));
        }
        session.settle(Duration::from_millis(500), Duration::from_secs(5)).await?;
        Ok(session)
    }

    /// Ms since the session began.
    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn record(&mut self, kind: EventKind) {
        let at_ms = self.elapsed_ms();
        self.events.push(Event { at_ms, kind });
    }

    /// The index to give `events_since` later.
    pub fn mark(&self) -> usize {
        self.events.len()
    }

    pub fn events_since(&self, mark: usize) -> &[Event] {
        &self.events[mark.min(self.events.len())..]
    }

    /// Sends the connection state the client has reached, once.
    async fn report(&mut self, state: u8) -> Result<(), String> {
        if state > self.client_state {
            self.client_state = state;
            self.channel.send_reliable(&wire::view_change_state(state)).await?;
        }
        Ok(())
    }

    /// Sends messages (already framed) reliably.
    pub async fn send(&mut self, messages: &[u8]) -> Result<(), String> {
        self.channel.send_reliable(messages).await.map(|_| ())
    }

    /// Awaits the channel until `done` holds or `timeout` passes. Returns whether it held.
    pub async fn run_until(&mut self, timeout: Duration, mut done: impl FnMut(&Session) -> bool) -> Result<bool, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if done(self) {
                return Ok(true);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            self.channel.resend_due().await?;
            if self.last_ping.elapsed() >= KEEP_ALIVE_EVERY {
                self.last_ping = Instant::now();
                self.channel.send_ping(self.elapsed_ms()).await?;
            }
            for received in self.channel.poll(POLL.min(left)).await? {
                self.handle(received).await?;
            }
            if let Some((reason, text)) = &self.disconnected {
                if !done(self) {
                    return Err(format!("the server disconnected the bot ({reason:#x}: {text})"));
                }
            }
        }
    }

    /// Awaits the channel for `time`, whatever happens.
    pub async fn pump(&mut self, time: Duration) -> Result<(), String> {
        self.run_until(time, |_| false).await.map(|_| ())
    }

    /// Awaits until nothing new has been recorded for `quiet`, or `max` has passed.
    pub async fn settle(&mut self, quiet: Duration, max: Duration) -> Result<(), String> {
        let start = Instant::now();
        loop {
            let seen = (self.events.len(), self.snapshot_updates);
            self.pump(quiet).await?;
            if seen == (self.events.len(), self.snapshot_updates) || start.elapsed() >= max {
                return Ok(());
            }
        }
    }

    async fn handle(&mut self, received: Received) -> Result<(), String> {
        match received {
            Received::Message(id, payload) => self.handle_message(id, &payload).await,
            Received::Pong { sent, server_clock } => {
                self.pongs.push((sent, server_clock));
                Ok(())
            }
            Received::DisconnectAck => {
                self.disconnect_acked = true;
                Ok(())
            }
            Received::Disconnect { reason, text } => {
                self.record(EventKind::Disconnect { reason, text: text.clone() });
                self.disconnected = Some((reason, text));
                Ok(())
            }
            Received::Unknown(_) => Ok(()),
        }
    }

    async fn handle_message(&mut self, id: u32, payload: &[u8]) -> Result<(), String> {
        *self.message_counts.entry(id).or_default() += 1;
        match id {
            wire::MSG_SHARD_ID => {
                let mut r = Reader::new(payload);
                if let (Some(shard), Some(name)) = (r.string(), r.string()) {
                    self.shard = Some((shard, name));
                }
            }
            wire::MSG_GAME_CONTEXT_INFO => {
                let mut r = Reader::new(payload);
                if r.take(16).is_some() {
                    if let (Some(map), Some(mode)) = (r.string(), r.string()) {
                        self.context = Some((map, mode));
                    }
                }
            }
            wire::MSG_DEBUG_AUTHORITY_CHANGE_INFO => {
                self.server_name = Reader::new(payload).string();
            }
            wire::MSG_VIEW_CHANGE_STATE => {
                if let Some(&state) = payload.first() {
                    self.server_state = self.server_state.max(state);
                    self.record(EventKind::State { state });
                    // What the real client reports as the server moves it: it spawns the universe root after 14, and
                    // reaches each later state when the server names it.
                    match state {
                        14 => self.report(10).await?,
                        15..=17 => self.report(state).await?,
                        _ => {}
                    }
                }
            }
            wire::MSG_BIND_BATCH_ADD_BIND => match Bind::parse(payload) {
                Some(bind) => {
                    self.record(EventKind::Bind { entity: bind.entity_id, class_crc: bind.class_crc, parent: bind.parent_id, attach: bind.parent_attach_id, batch: bind.batch });
                    self.world.bind(bind);
                }
                None => return Err(format!("an unreadable bind of {} bytes: {}", payload.len(), wire::hex(&payload[..payload.len().min(64)]))),
            },
            wire::MSG_BIND_BATCH_WAITING => {
                // The client spawns the batch and says so; the server then ends it.
                if let Some(batch) = payload.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap())) {
                    self.channel.send_reliable(&wire::spawns_completed(batch)).await?;
                }
            }
            wire::MSG_BIND_BATCH_END => {
                let batch = payload.get(..4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
                self.record(EventKind::BatchEnd { batch });
                self.batch_ends += 1;
                match self.batch_ends {
                    1 => self.report(11).await?,
                    2 => self.report(14).await?,
                    _ => {}
                }
            }
            wire::MSG_UNBIND_BATCH | wire::MSG_UNBIND_BATCH_BEGIN | wire::MSG_UNBIND_BATCH_ADD | wire::MSG_UNBIND_BATCH_END => {
                if let Some((_, entities)) = wire::parse_unbind(payload) {
                    for entity in entities {
                        self.record(EventKind::Unbind { entity });
                        self.world.unbind(entity);
                    }
                }
            }
            wire::MSG_ENTITY_REMOTE_METHOD => {
                if let Some((entity, method, args)) = wire::parse_remote_method(payload) {
                    self.record(EventKind::Method { entity, method, args_hex: wire::hex(&args) });
                }
            }
            wire::MSG_ENTITY_TOKEN_EVENT => {
                if let Some((entity, token, request, decision)) = wire::parse_token_event(payload) {
                    self.record(EventKind::Token { entity, token, request, decision });
                }
            }
            wire::MSG_UPDATE_BUBBLE_CREATE => self.record(EventKind::Bubble),
            wire::MSG_ENTITY_SNAPSHOT_UPDATE => self.snapshot_updates += 1,
            _ => {}
        }
        Ok(())
    }

    /// Sends a ping and waits for its pong; returns the server's clock.
    pub async fn ping(&mut self, timeout: Duration) -> Result<Option<u64>, String> {
        let time = 0x1122_3344_0000 + self.pongs.len() as u64;
        self.channel.send_ping(time).await?;
        let found = self.run_until(timeout, |s| s.pongs.iter().any(|(sent, _)| *sent == time)).await?;
        Ok(found.then(|| self.pongs.iter().find(|(sent, _)| *sent == time).map(|(_, clock)| *clock)).flatten())
    }

    /// Calls `method` on `entity` as the client does.
    pub async fn call(&mut self, entity: u64, method: u32, args: &[u8]) -> Result<(), String> {
        self.send(&wire::remote_method(entity, method, args)).await
    }

    /// The args of the first remote method from the server on `entity` with `method` since `mark`.
    pub fn method_since(&self, mark: usize, entity: u64, method: u32) -> Option<Vec<u8>> {
        self.events_since(mark).iter().find_map(|event| match &event.kind {
            EventKind::Method { entity: e, method: m, args_hex } if *e == entity && *m == method => Some(wire::unhex(args_hex)),
            _ => None,
        })
    }

    /// Leaves as a client does when it exits to the menu: a disconnect notice, then the wait for the shard's answer.
    pub async fn leave(&mut self, reason: u32, text: &str, timeout: Duration) -> Result<bool, String> {
        self.channel.send_disconnect(reason, text).await?;
        let _ = self.run_until(timeout, |s| s.disconnect_acked).await;
        Ok(self.disconnect_acked)
    }

    /// Sends one more disconnect notice, as a client does until it is answered.
    pub async fn leave_again(&mut self, reason: u32, text: &str, timeout: Duration) -> Result<bool, String> {
        self.disconnect_acked = false;
        self.leave(reason, text, timeout).await
    }

    /// Ends the Hub connection, which tells the Hub the player left.
    pub fn close_hub(&mut self) {
        self.hub = None;
    }

    pub fn packets(&self) -> (u64, u64) {
        (self.channel.packets_sent, self.channel.packets_received)
    }
}
