//! Entity tokens a live client asks for, in particular the access token of an inventory container. Before the client uses an
//! inventory that is not the personal one (the container of an item it wears, or of one it opens) it calls
//! `CInventoryContainerComponent::RequestAccessToInventory`, which logs `<Inventory Token Flow> Requesting access token for
//! User[..] on Inventory[..]` and sends an `EntityTokenRequest` (replication message 21) for the container component's
//! `m_userToken`, then queues every request on that inventory until the token is granted. The answer is an `EntityTokenEvent`
//! (message 23): granted (1) or denied (0). Nothing answered it, so the requests of the inventory stayed pending.
//!
//! The DGS has authority over a live player's gear and decides: the token is granted for an item the player wears (the DGS's
//! own tree, `worn`) or owns (sc-entitygraph, which owns that data), and denied for an item another player owns. See "Inventory
//! access tokens" in docs/INVENTORY.md for how the request reads.

use std::time::Duration;

use sc_api::internal::{entity_graph_internal_client::EntityGraphInternalClient, entity_token_response::Decision, EntityTokenRequest, GetOwnersRequest};
use tonic::{transport::Channel, Status};
use tracing::{info, warn};

use crate::worn::Worn;

/// How long the DGS waits for sc-entitygraph before it leaves the request unanswered.
const OWNER_TIMEOUT: Duration = Duration::from_secs(3);

/// The byte after the request of a user token such as an inventory's access token. The arbitrated tokens a client asks for as
/// an entity binds end in `0xce`; the bit is what the DGS tests.
pub const USER_TOKEN_BIT: u32 = 1;

/// Who owns an entity, as the items' owner (the game's `ownerId`).
#[allow(async_fn_in_trait)]
pub trait Owners {
    /// `None` for an entity that does not exist or has no owner.
    async fn owner_of(&self, entity: u64) -> Result<Option<u64>, Status>;
}

/// `Owners` over sc-entitygraph, `None` without one.
pub struct GraphOwners(pub Option<EntityGraphInternalClient<Channel>>);

impl Owners for GraphOwners {
    async fn owner_of(&self, entity: u64) -> Result<Option<u64>, Status> {
        let mut graph = self.0.clone().ok_or_else(|| Status::unavailable("this DGS has no sc-entitygraph"))?;
        let call = graph.get_owners(GetOwnersRequest { entities: vec![entity] });
        let reply = tokio::time::timeout(OWNER_TIMEOUT, call).await.map_err(|_| Status::deadline_exceeded("sc-entitygraph did not answer"))??.into_inner();
        Ok(reply.owners.iter().find(|owner| owner.entity == entity).map(|owner| owner.owner).filter(|owner| *owner != 0))
    }
}

/// What to answer `request` of a player whose worn items are `worn` (`None` when this DGS does not hold the player for the
/// session). Logs the request and the answer.
pub async fn decide(owners: &impl Owners, worn: Option<&Worn>, request: &EntityTokenRequest) -> Decision {
    let (player, entity) = (request.player_id, request.entity_id);
    let describe = |decision: &str, why: &str| {
        info!(
            player, entity, token = request.token, request = request.request, flags = format_args!("{:#04x}", request.flags), decision, why,
            "entity token request"
        );
    };
    if request.flags & USER_TOKEN_BIT == 0 {
        describe("none", "not a user token, left as before");
        return Decision::Unspecified;
    }
    let Some(worn) = worn else {
        warn!(player, entity, token = request.token, "a token request of a player this DGS does not hold for the session");
        return Decision::Unspecified;
    };
    if worn.contains(entity) {
        describe("granted", "worn by the player");
        return Decision::Granted;
    }
    match owners.owner_of(entity).await {
        Ok(Some(owner)) if owner == player => {
            describe("granted", "owned by the player");
            Decision::Granted
        }
        Ok(Some(owner)) => {
            info!(player, entity, owner, token = request.token, request = request.request, decision = "denied", "entity token request: owned by someone else");
            Decision::Denied
        }
        Ok(None) => {
            describe("none", "the entity has no owner");
            Decision::Unspecified
        }
        Err(status) => {
            warn!(player, entity, %status, "entity token request: the owner could not be read, left unanswered");
            Decision::Unspecified
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        loadout::{loadout_binds, DEFAULT_LOADOUT},
        player::player_index,
    };

    const PLAYER: u64 = 300_000_100_001;

    struct Fake(Result<Option<u64>, Status>);

    impl Owners for Fake {
        async fn owner_of(&self, _entity: u64) -> Result<Option<u64>, Status> {
            self.0.clone()
        }
    }

    fn worn() -> (Worn, u64) {
        let binds = loadout_binds(PLAYER, player_index(1_000_000).unwrap(), &DEFAULT_LOADOUT);
        let first = binds[0].entity_id;
        (Worn::from_binds(PLAYER, &binds), first)
    }

    /// The two requests of the 2026-11-09 session: the container component's `m_userToken` (index 2, request 1, `0xcf`) of the
    /// worn core 400000000028, and the arbitrated token 0 (`0xce`) the client asks for as it binds.
    fn request(entity: u64, token: u32, flags: u32) -> EntityTokenRequest {
        EntityTokenRequest { replicant: 1, session: 1, player_id: PLAYER, entity_id: entity, token, request: 1, flags }
    }

    #[tokio::test]
    async fn the_access_token_of_a_worn_item_is_granted() {
        let (worn, item) = worn();
        assert_eq!(decide(&Fake(Err(Status::unavailable("no"))), Some(&worn), &request(item, 2, 0xcf)).await, Decision::Granted);
    }

    #[tokio::test]
    async fn the_access_token_of_an_item_the_player_owns_is_granted() {
        let (worn, _) = worn();
        assert_eq!(decide(&Fake(Ok(Some(PLAYER))), Some(&worn), &request(400_000_000_900, 2, 0xcf)).await, Decision::Granted);
    }

    #[tokio::test]
    async fn the_access_token_of_an_item_of_someone_else_is_denied() {
        let (worn, _) = worn();
        assert_eq!(decide(&Fake(Ok(Some(300_000_100_002))), Some(&worn), &request(400_000_000_900, 2, 0xcf)).await, Decision::Denied);
    }

    #[tokio::test]
    async fn what_cannot_be_decided_stays_unanswered() {
        let (worn, item) = worn();
        // An arbitrated token (0xce) of a worn item, as the client asks as it binds.
        assert_eq!(decide(&Fake(Ok(Some(PLAYER))), Some(&worn), &request(item, 0, 0xce)).await, Decision::Unspecified);
        assert_eq!(decide(&Fake(Ok(None)), Some(&worn), &request(400_000_000_900, 2, 0xcf)).await, Decision::Unspecified);
        assert_eq!(decide(&Fake(Err(Status::unavailable("no"))), Some(&worn), &request(400_000_000_900, 2, 0xcf)).await, Decision::Unspecified);
        assert_eq!(decide(&Fake(Ok(Some(PLAYER))), None, &request(item, 2, 0xcf)).await, Decision::Unspecified);
    }
}
