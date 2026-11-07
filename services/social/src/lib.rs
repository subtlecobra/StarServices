//! What the social services share that is not data: account lookups, pushes, pagination, URNs, game
//! modes, group roles, transactions, and clients of the services that own the data. Each service owns
//! its tables, creates them at start through [`Context::open`], and is the only one that reads or
//! writes them; the others ask it over gRPC through [`peers::Peers`]:
//!
//! | owner | tables |
//! |---|---|
//! | sc-social | `social_groups`, `social_group_members`, `social_invitations`, `social_group_cleanups` |
//! | sc-chat | `chat_messages` |
//! | sc-squad | `squads` |
//! | sc-custom-lobby | `custom_lobbies` |
//! | sc-rtc | `rtc_channels`, `rtc_calls` |
//! | sc-presence | `player_presence` |
//! | sc-contacts | `contact_relationships`, `contact_friend_requests`, `contact_notes` |

pub mod accounts;
pub mod groups;
pub mod matchmaking;
pub mod modes;
pub mod page;
pub mod peers;
pub mod presence;
pub mod push;
#[doc(hidden)]
pub mod testing;
pub mod tx;
pub mod urn;

use std::time::{SystemTime, UNIX_EPOCH};

use accounts::Directory;
use peers::{PeerArgs, Peers};
use push::Push;
use sc_db::{DatabaseArgs, Postgres};
use sc_runtime::{
    caller,
    state::{Account, State},
    CommonArgs,
};
use tokio::sync::Notify;
use tonic::{metadata::MetadataMap, Status};

/// Everything a social service handler needs.
pub struct Context {
    pub state: State,
    pub directory: Directory,
    pub db: Postgres,
    pub push: Push,
    pub peers: Peers,
    /// Notified after a transaction that asked for it with
    /// [`Tx::notify_after_commit`](tx::Tx::notify_after_commit) has committed.
    pub committed: Notify,
}

impl Context {
    /// `schema` holds the tables of the service itself, which it alone reads and writes.
    pub async fn open(
        common: &CommonArgs,
        database: &DatabaseArgs,
        push: &str,
        peers: &PeerArgs,
        schema: &str,
    ) -> Result<Self, String> {
        Ok(Self {
            state: State::load(&common.data)?,
            directory: Directory::load(&common.data)?,
            db: Postgres::connect(&database.postgres, schema).await?,
            push: Push::connect(push)?,
            peers: Peers::connect(peers)?,
            committed: Notify::new(),
        })
    }

    pub fn caller(&self, metadata: &MetadataMap) -> Result<Account, Status> {
        caller(&self.state, metadata).ok_or_else(|| Status::unauthenticated("missing or invalid token"))
    }

    /// The key the client passes back when it subscribes to push topics. A token carries the
    /// account, so the push service can verify it.
    pub fn subscription_key(&self, account: &Account) -> String {
        self.state.issue_token(account)
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}
