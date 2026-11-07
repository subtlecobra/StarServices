//! What every service needs to work with a group: its settings by type, the roles of its members, and
//! pushes to them. The groups themselves are sc-social's, the only service that reads or writes them;
//! the others ask it through [`Peers`](crate::peers::Peers).

use sc_proto::{
    internal::{GroupMemberAccount, InternalGroup},
    services::social::v1::{ChatSettings, Group, GroupSettings, GroupType, InviteMode, RtcSettings},
};
use sc_runtime::state::Account;
use tonic::Status;

use crate::{
    push::{group_topic, player_topic, Event},
    tx::Outbox,
    urn,
};

const MAX_MEMBERS: i32 = 100;
const MAX_PARTY_MEMBERS: i32 = 20;

/// One member, as the services need it to reach the player.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub geid: u64,
    pub account_id: u32,
    pub name: String,
}

/// A group and its members in the order they joined.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroupView {
    pub group: Group,
    pub members: Vec<Member>,
}

impl From<InternalGroup> for GroupView {
    fn from(internal: InternalGroup) -> Self {
        let members = internal
            .members
            .into_iter()
            .map(|m| Member { geid: m.geid, account_id: m.account_id, name: m.name })
            .collect();
        Self { group: internal.group.unwrap_or_default(), members }
    }
}

impl From<GroupView> for InternalGroup {
    fn from(view: GroupView) -> Self {
        let members = view
            .members
            .into_iter()
            .map(|m| GroupMemberAccount { geid: m.geid, account_id: m.account_id, name: m.name })
            .collect();
        Self { group: Some(view.group), members }
    }
}

pub fn settings(kind: GroupType) -> GroupSettings {
    let base = GroupSettings {
        min_member_count: 1,
        max_member_count: MAX_MEMBERS,
        can_leave: true,
        requires_leader: true,
        rtc: Some(RtcSettings { enabled: true, positional: false }),
        chat: Some(ChatSettings { enabled: true }),
        invite_mode: InviteMode::LeaderOnly as i32,
        presence_aware: true,
        ..Default::default()
    };
    match kind {
        GroupType::Dm => GroupSettings {
            min_member_count: 2,
            max_member_count: 2,
            can_leave: false,
            requires_leader: false,
            invite_mode: InviteMode::None as i32,
            ..base
        },
        GroupType::Party => GroupSettings {
            min_member_count: 2,
            max_member_count: MAX_PARTY_MEMBERS,
            unique_for_type: true,
            ..base
        },
        GroupType::Squad | GroupType::CustomLobby => GroupSettings { unique_for_type: true, ..base },
        GroupType::CustomLobbyTeam | GroupType::Team => {
            GroupSettings { min_member_count: 0, invite_mode: InviteMode::None as i32, requires_leader: false, ..base }
        }
        _ => base,
    }
}

pub fn is_member(group: &Group, geid: u64) -> bool {
    let member = urn::player(geid);
    group.members.iter().any(|m| m.urn == member)
}

pub fn is_leader(group: &Group, geid: u64) -> bool {
    group.leader_urn == urn::player(geid)
}

/// The leader and moderators run a group.
pub fn can_manage(group: &Group, geid: u64) -> bool {
    is_leader(group, geid) || group.moderators.contains(&urn::player(geid))
}

pub fn require_leader(group: &Group, geid: u64) -> Result<(), Status> {
    if is_leader(group, geid) {
        Ok(())
    } else {
        Err(Status::permission_denied("only the group leader can do this"))
    }
}

/// Sends `event` to each member on the group's topic.
pub fn to_members(out: &impl Outbox, id: &str, members: &[Member], event: &Event) {
    for member in members {
        out.send(member.account_id, group_topic(id), event);
    }
}

/// Sends `event` to a player on its own topic.
pub fn to_player(out: &impl Outbox, member: &Member, event: &Event) {
    out.send(member.account_id, player_topic(member.geid), event);
}

/// Delivers on the topic the client follows for this kind of group: it subscribes to every group
/// except direct messages, for which it listens on the player topic.
pub fn to_group(out: &impl Outbox, group: &Group, members: &[Member], event: &Event) {
    if group.r#type == GroupType::Dm as i32 {
        members.iter().for_each(|member| to_player(out, member, event));
    } else {
        to_members(out, &group.id, members, event);
    }
}

pub fn member_of_account(account: &Account) -> Member {
    Member { geid: account.geid, account_id: account.account_id, name: account.handle.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_proto::services::social::v1::GroupMember;

    fn group(leader: u64, moderator: u64) -> Group {
        Group {
            leader_urn: urn::player(leader),
            moderators: vec![urn::player(moderator)],
            members: vec![GroupMember { urn: urn::player(leader), ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn roles_follow_the_urns() {
        let group = group(1, 2);
        assert!(is_member(&group, 1) && !is_member(&group, 2));
        assert!(can_manage(&group, 1) && can_manage(&group, 2) && !can_manage(&group, 3));
        assert!(require_leader(&group, 1).is_ok() && require_leader(&group, 2).is_err());
    }

    #[test]
    fn direct_messages_have_no_leader_and_cannot_be_left() {
        let settings = settings(GroupType::Dm);
        assert_eq!(settings.max_member_count, 2);
        assert!(!settings.can_leave && !settings.requires_leader);
        assert_eq!(settings.invite_mode(), InviteMode::None);
    }

    #[test]
    fn a_party_is_unique_and_needs_a_second_player() {
        let settings = settings(GroupType::Party);
        assert!(settings.unique_for_type && settings.requires_leader);
        assert_eq!(settings.min_member_count, 2);
    }

    #[test]
    fn a_group_survives_the_internal_message() {
        let view = GroupView {
            group: Group { id: "g".to_string(), ..group(1, 2) },
            members: vec![Member { geid: 1, account_id: 10, name: "One".to_string() }],
        };
        assert_eq!(GroupView::from(InternalGroup::from(view.clone())), view);
    }
}
