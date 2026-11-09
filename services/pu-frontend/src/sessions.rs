//! Who a connecting client is. Its connect request carries the session the Hub issued it at login (32 random hex digits,
//! as the real backend's), which says nothing by itself: only the Hub knows whose it is (`HubInternal.ResolveSession`).
//! This is the shard's side of the real backend's "Expect Incoming Connection": a player's identity comes from the
//! backend, never from what the client claims.
//!
//! The lookup is asynchronous and the packet loop is not: the first connect request of a session starts it and gets no
//! answer, and a resend once the Hub has answered is accepted (or, for a session the Hub does not know, never is). The
//! client resends every 400 ms for 30 seconds, so this costs the first resend at most.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use sc_api::internal::{hub_internal_client::HubInternalClient, ResolveSessionRequest};
use tonic::transport::{Channel, Endpoint};
use tracing::{info, warn};

use crate::link::Identity;

/// How long the Hub may take to answer.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an answer is kept, longer than the client's 30 seconds of connect requests.
const KEEP: Duration = Duration::from_secs(120);

#[derive(Clone, Copy)]
enum Answer {
    Pending,
    Known(Identity),
    Refused,
}

/// The Hub's answers, by session id, with when each was given.
type Answers = HashMap<Vec<u8>, (Answer, Instant)>;

#[derive(Clone)]
pub struct Sessions {
    /// None only in tests, which name sessions as the Hub once did (`hub-<account>[-<home location>]`).
    hub: Option<HubInternalClient<Channel>>,
    answers: Arc<Mutex<Answers>>,
}

impl Sessions {
    pub fn new(hub: &str) -> Result<Self, String> {
        let endpoint = Endpoint::from_shared(hub.to_string()).map_err(|e| format!("--hub {hub}: {e}"))?;
        Ok(Self { hub: Some(HubInternalClient::new(endpoint.connect_lazy())), answers: Arc::default() })
    }

    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self { hub: None, answers: Arc::default() }
    }

    /// As if the Hub had refused `session`.
    #[cfg(test)]
    pub fn refuse(&self, session: &[u8]) {
        self.answers.lock().unwrap().insert(session.to_vec(), (Answer::Refused, Instant::now()));
    }

    /// The identity of `session`, once the Hub has said whose it is. The first call asks the Hub; until it answers, and
    /// for a session it refused, this is None and the client gets no answer.
    pub fn identity(&self, session: &[u8]) -> Option<Identity> {
        let Some(hub) = self.hub.clone() else { return test_identity(session) };
        let mut answers = self.answers.lock().unwrap();
        answers.retain(|_, (_, at)| at.elapsed() < KEEP);
        match answers.get(session) {
            Some((Answer::Known(identity), _)) => return Some(*identity),
            Some(_) => return None,
            None => {}
        }
        answers.insert(session.to_vec(), (Answer::Pending, Instant::now()));
        drop(answers);
        let sessions = self.clone();
        let session = session.to_vec();
        tokio::spawn(async move {
            let answer = ask(hub, &session).await;
            let mut answers = sessions.answers.lock().unwrap();
            match answer {
                Some(answer) => {
                    answers.insert(session, (answer, Instant::now()));
                }
                // A failure of the Hub, not a refusal: the next connect request asks again.
                None => {
                    answers.remove(&session);
                }
            }
        });
        None
    }
}

/// The Hub's answer: Known or Refused, or None when it could not answer.
async fn ask(mut hub: HubInternalClient<Channel>, session: &[u8]) -> Option<Answer> {
    let Ok(text) = std::str::from_utf8(session) else {
        warn!(bytes = session.len(), "a session that is not text: refused");
        return Some(Answer::Refused);
    };
    let request = ResolveSessionRequest { session: text.to_string() };
    match tokio::time::timeout(LOOKUP_TIMEOUT, hub.resolve_session(request)).await {
        Ok(Ok(response)) => {
            let response = response.into_inner();
            if !response.found {
                warn!(length = text.len(), "a session the Hub does not know: refused");
                return Some(Answer::Refused);
            }
            let location = (response.home_location != 0).then_some(response.home_location);
            info!(account = response.account_id, nickname = %response.nickname, home = location, "session resolved");
            Some(Answer::Known(Identity { account: Some(u64::from(response.account_id)), location, geid: (response.geid != 0).then_some(response.geid) }))
        }
        Ok(Err(status)) => {
            warn!(error = status.message(), "session lookup failed");
            None
        }
        Err(_) => {
            warn!("session lookup timed out");
            None
        }
    }
}

/// Tests name sessions `hub-<account>[-<home location>]`, and any other session has no account.
#[cfg(test)]
pub fn test_identity(session: &[u8]) -> Option<Identity> {
    let fields = std::str::from_utf8(session).ok().and_then(|text| text.strip_prefix("hub-"));
    let (account, location) = match fields {
        Some(fields) => fields.split_once('-').map_or((fields, None), |(account, home)| (account, Some(home))),
        None => return Some(Identity { account: None, location: None, geid: None }),
    };
    Some(Identity { account: account.parse().ok(), location: location.and_then(|home| home.parse().ok()), geid: None })
}

#[cfg(not(test))]
fn test_identity(_: &[u8]) -> Option<Identity> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub_sessions() -> Sessions {
        Sessions::new("http://127.0.0.1:1").unwrap()
    }

    #[tokio::test]
    async fn a_known_session_gives_its_identity() {
        let sessions = hub_sessions();
        let identity = Identity { account: Some(1_100_001), location: Some(1_105_811_199), geid: None };
        sessions.answers.lock().unwrap().insert(b"0123".to_vec(), (Answer::Known(identity), Instant::now()));
        let found = sessions.identity(b"0123").unwrap();
        assert_eq!((found.account, found.location), (Some(1_100_001), Some(1_105_811_199)));
    }

    #[tokio::test]
    async fn a_pending_or_refused_session_gives_none() {
        let sessions = hub_sessions();
        sessions.answers.lock().unwrap().insert(b"pending".to_vec(), (Answer::Pending, Instant::now()));
        sessions.refuse(b"refused");
        assert!(sessions.identity(b"pending").is_none());
        assert!(sessions.identity(b"refused").is_none());
    }

    #[test]
    fn tests_name_their_sessions_as_the_hub_once_did() {
        let identity = test_identity(b"hub-1000001-2026442305").unwrap();
        assert_eq!((identity.account, identity.location), (Some(1_000_001), Some(2_026_442_305)));
        assert_eq!(test_identity(b"check").unwrap().account, None);
    }
}
