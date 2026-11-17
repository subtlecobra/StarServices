# Chat

Messages belong to sc-chat (`chat_messages`), groups and their members to sc-social. A message goes to a group the sender
is a member of and whose settings enable chat; sc-push delivers it to the group's members.

## Global chat

The global chat is a group of type `GROUP_TYPE_SERVER` named `Global` that every account belongs to. It needs no service
of its own: sc-social already owns membership, sc-chat the messages and sc-push the delivery.

- sc-social creates the group the first time any player lists its groups (`ListGroups` with no type filter, or with
  `GROUP_TYPE_SERVER`), serialized between replicas with an advisory lock, and puts that player in it
  (`store::ensure_global`). Only the joining player is sent `JoinedGroup`; the others are not told about each arrival.
- Settings (`groups::settings`): no leader, cannot be left, no invitations, no voice channel, no presence tracking of the
  members, at most 100 000 members.
- Chat needs nothing new: `SendMessage` checks membership and the group's chat setting as for any group.

### When another service would be needed

Not now. The group's members receive every message, so the fan-out grows with the number of accounts, and a group row
lists every member when it is loaded. For a private backend that is fine. A service of its own becomes worth it when:

- the population reaches the tens of thousands: channel fan-out would move out of sc-social/sc-push into a service that
  keeps channels as subscriptions, not as group members;
- chat should differ by place (per star system, per shard, per language): channels keyed by location need the DGS or Hub
  to say where a player is, which sc-social does not know;
- moderation (mutes, filters, rate limits) goes beyond erasing a message.

Until then those would be additions to sc-chat (rate limit, mute list) and not a new service.
