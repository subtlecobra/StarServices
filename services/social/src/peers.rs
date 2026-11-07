//! The services that own what the social services share, and how to ask them. Each table has one
//! owner, the only service that reads or writes it: sc-social keeps groups (`social_*`), sc-presence
//! `player_presence`, sc-contacts `contact_*`, and sc-chat, sc-squad, sc-custom-lobby and sc-rtc their
//! rows per group. Everyone else goes through these clients, which connect on first use.

use std::{collections::HashMap, time::Duration};

use clap::Args;
use sc_proto::{
    internal::{
        contacts_internal_client::ContactsInternalClient, group_data_internal_client::GroupDataInternalClient,
        presence_internal_client::PresenceInternalClient, social_internal_client::SocialInternalClient,
        CreateGroupRequest, DeleteGroupDataRequest, DestroyGroupRequest, GetGroupsRequest, GetPresencesRequest,
        GetTeamsRequest, IsBlockedRequest, JoinGroupRequest, ListFriendsRequest, ListGroupIdsRequest,
        MoveToTeamRequest, PlayerPresence, SyncTeamsRequest,
    },
    services::social::v1::GroupType,
};
use tonic::{
    transport::{Channel, Endpoint},
    Status,
};

use crate::groups::{is_member, GroupView};

pub const SOCIAL: &str = "http://sc-social:50051";
pub const PRESENCE: &str = "http://sc-presence:50051";
pub const CONTACTS: &str = "http://sc-contacts:50051";
/// Every service with rows per group, as `name=address`.
pub const GROUP_DATA: &str =
    "chat=http://sc-chat:50051,squad=http://sc-squad:50051,custom_lobby=http://sc-custom-lobby:50051,rtc=http://sc-rtc:50051";

const TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Args, Clone, Debug)]
pub struct PeerArgs {
    /// Address of sc-social, which owns groups.
    #[arg(long, env = "SCBACKEND_SOCIAL", default_value = SOCIAL)]
    pub social: String,
    /// Address of sc-presence, which owns presence.
    #[arg(long, env = "SCBACKEND_PRESENCE", default_value = PRESENCE)]
    pub presence: String,
    /// Address of sc-contacts, which owns relationships.
    #[arg(long, env = "SCBACKEND_CONTACTS", default_value = CONTACTS)]
    pub contacts: String,
    /// The services that keep rows per group, as `name=address` separated by commas. sc-social has
    /// them delete what belonged to a group it destroyed.
    #[arg(long, env = "SCBACKEND_GROUP_DATA", default_value = GROUP_DATA)]
    pub group_data: String,
}

impl Default for PeerArgs {
    fn default() -> Self {
        Self {
            social: SOCIAL.to_string(),
            presence: PRESENCE.to_string(),
            contacts: CONTACTS.to_string(),
            group_data: GROUP_DATA.to_string(),
        }
    }
}

/// Clients of the owners. Cloning a client is cheap: they share one connection.
pub struct Peers {
    pub social: SocialInternalClient<Channel>,
    pub presence: PresenceInternalClient<Channel>,
    pub contacts: ContactsInternalClient<Channel>,
    pub group_data: Vec<(String, GroupDataInternalClient<Channel>)>,
}

fn channel(flag: &str, address: &str) -> Result<Channel, String> {
    let endpoint = Endpoint::from_shared(address.to_string()).map_err(|e| format!("--{flag} {address}: {e}"))?;
    Ok(endpoint.timeout(TIMEOUT).connect_timeout(CONNECT_TIMEOUT).connect_lazy())
}

/// `name=address` pairs, separated by commas.
fn pairs(list: &str) -> Result<Vec<(String, String)>, String> {
    list.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (name, address) = entry.split_once('=').ok_or_else(|| format!("--group-data: {entry} is not name=address"))?;
            Ok((name.trim().to_string(), address.trim().to_string()))
        })
        .collect()
}

impl Peers {
    /// Must run inside the runtime. Nothing connects until it is first used.
    pub fn connect(args: &PeerArgs) -> Result<Self, String> {
        let group_data = pairs(&args.group_data)?
            .into_iter()
            .map(|(name, address)| Ok((name, GroupDataInternalClient::new(channel("group-data", &address)?))))
            .collect::<Result<_, String>>()?;
        Ok(Self {
            social: SocialInternalClient::new(channel("social", &args.social)?),
            presence: PresenceInternalClient::new(channel("presence", &args.presence)?),
            contacts: ContactsInternalClient::new(channel("contacts", &args.contacts)?),
            group_data,
        })
    }

    /// The groups with these ids, in that order. Ids of no group are skipped.
    pub async fn groups(&self, ids: &[String]) -> Result<Vec<GroupView>, Status> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let request = GetGroupsRequest { group_ids: ids.to_vec() };
        let groups = self.social.clone().get_groups(request).await?.into_inner().groups;
        Ok(groups.into_iter().map(GroupView::from).collect())
    }

    pub async fn group(&self, id: &str) -> Result<GroupView, Status> {
        let found = self.groups(&[id.to_string()]).await?.pop();
        found.ok_or_else(|| Status::not_found(format!("no group {id}")))
    }

    /// Whether the group is still there. A service that wrote a row for a group asks after the write:
    /// a group destroyed before then has already had its rows deleted, so the writer deletes its own.
    pub async fn exists(&self, id: &str) -> Result<bool, Status> {
        Ok(!self.groups(&[id.to_string()]).await?.is_empty())
    }

    /// Whether sc-social says the group is gone, for a writer that checks after its write. Without an
    /// answer the row stays: sc-social did answer for the same group a moment before.
    pub async fn gone(&self, id: &str) -> bool {
        match self.exists(id).await {
            Ok(exists) => !exists,
            Err(error) => {
                tracing::warn!(group = id, %error, "could not check that the group still exists");
                false
            }
        }
    }

    /// The group, if `geid` is in it.
    pub async fn member_of(&self, id: &str, geid: u64) -> Result<GroupView, Status> {
        let view = self.group(id).await?;
        if is_member(&view.group, geid) {
            Ok(view)
        } else {
            Err(Status::permission_denied("not a member of this group"))
        }
    }

    /// Ids of the groups `geid` is in, oldest membership first, limited to `kinds` unless that is empty.
    pub async fn group_ids(&self, geid: u64, kinds: &[i32]) -> Result<Vec<String>, Status> {
        let request = ListGroupIdsRequest { geid, types: kinds.to_vec() };
        Ok(self.social.clone().list_group_ids(request).await?.into_inner().group_ids)
    }

    /// The one group of `kind` this player is in, such as its squad.
    pub async fn one_of(&self, geid: u64, kind: GroupType) -> Result<Option<GroupView>, Status> {
        match self.group_ids(geid, &[kind as i32]).await?.first() {
            Some(id) => self.group(id).await.map(Some),
            None => Ok(None),
        }
    }

    pub async fn teams(&self, id: &str) -> Result<Vec<GroupView>, Status> {
        let request = GetTeamsRequest { group_id: id.to_string() };
        let teams = self.social.clone().get_teams(request).await?.into_inner().groups;
        Ok(teams.into_iter().map(GroupView::from).collect())
    }

    pub async fn create_group(
        &self,
        kind: GroupType,
        name: &str,
        parent_urn: &str,
        member_geids: Vec<u64>,
    ) -> Result<GroupView, Status> {
        let request =
            CreateGroupRequest { r#type: kind as i32, name: name.to_string(), parent_urn: parent_urn.to_string(), member_geids };
        let created = self.social.clone().create_group(request).await?.into_inner().group;
        created.map(GroupView::from).ok_or_else(|| Status::internal("sc-social returned no group"))
    }

    pub async fn join_group(&self, id: &str, geid: u64) -> Result<(), Status> {
        self.social.clone().join_group(JoinGroupRequest { group_id: id.to_string(), geid }).await?;
        Ok(())
    }

    /// Moves a player of a group to one of its teams. Returns the group and the team it left, which is
    /// `team_id` itself when the player was already there.
    pub async fn move_to_team(&self, team_id: &str, geid: u64) -> Result<(GroupView, String), Status> {
        let request = MoveToTeamRequest { team_id: team_id.to_string(), geid };
        let moved = self.social.clone().move_to_team(request).await?.into_inner();
        let group = moved.group.map(GroupView::from).ok_or_else(|| Status::internal("sc-social returned no group"))?;
        Ok((group, moved.previous_team_id))
    }

    pub async fn sync_teams(&self, id: &str, count: usize, team_type: GroupType) -> Result<(), Status> {
        let request = SyncTeamsRequest { group_id: id.to_string(), count: count as u32, team_type: team_type as i32 };
        self.social.clone().sync_teams(request).await?;
        Ok(())
    }

    pub async fn destroy_group(&self, id: &str) -> Result<(), Status> {
        self.social.clone().destroy_group(DestroyGroupRequest { group_id: id.to_string() }).await?;
        Ok(())
    }

    /// The presence of these accounts as others see it, by account. Accounts never online are left out.
    pub async fn presences(&self, accounts: &[u32]) -> Result<HashMap<u32, PlayerPresence>, Status> {
        if accounts.is_empty() {
            return Ok(HashMap::new());
        }
        let request = GetPresencesRequest { account_ids: accounts.to_vec() };
        let presences = self.presence.clone().get_presences(request).await?.into_inner().presences;
        Ok(presences.into_iter().map(|presence| (presence.account_id, presence)).collect())
    }

    /// Whether `target` refuses `from`.
    pub async fn is_blocked_by(&self, target: u32, from: u32) -> Result<bool, Status> {
        let request = IsBlockedRequest { owner_account_id: target, target_account_id: from };
        Ok(self.contacts.clone().is_blocked(request).await?.into_inner().blocked)
    }

    pub async fn friend_ids(&self, account: u32) -> Result<Vec<u32>, Status> {
        let request = ListFriendsRequest { account_id: account };
        Ok(self.contacts.clone().list_friends(request).await?.into_inner().account_ids)
    }

    /// Asks one service with rows per group to delete those of these groups.
    pub async fn delete_group_data(&self, target: &str, ids: &[String]) -> Result<u64, Status> {
        let (_, client) = self
            .group_data
            .iter()
            .find(|(name, _)| name == target)
            .ok_or_else(|| Status::not_found(format!("no group data service {target}")))?;
        let request = DeleteGroupDataRequest { group_ids: ids.to_vec() };
        Ok(client.clone().delete_group_data(request).await?.into_inner().deleted)
    }

    pub fn group_data_names(&self) -> Vec<String> {
        self.group_data.iter().map(|(name, _)| name.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_data_names_its_services() {
        let parsed = pairs(GROUP_DATA).unwrap();
        let names: Vec<&str> = parsed.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["chat", "squad", "custom_lobby", "rtc"]);
        assert_eq!(parsed[2].1, "http://sc-custom-lobby:50051");
        assert!(pairs("chat").is_err());
        assert!(pairs("").unwrap().is_empty());
    }
}
