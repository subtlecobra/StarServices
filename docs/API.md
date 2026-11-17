# Developer API

JSON over HTTP for the admin operations, for tools and scripts (sc-bot, shell scripts) that should not drive the
rendered admin site. It is served by `sc-admin-web` next to the pages, under `{prefix}/api/v1`; with the default prefix
that is `/admin/api/v1`, which the gateway already routes to sc-admin-web. It shares the pages' logic, so a request is
checked, limited and audited exactly as the page's action is: the same offline-player guard, the same class and stack
checks, the same `admin_audit` row with the actor, the same command keys (`admin:<request id>`) the owners apply once.

## Authentication

Access tokens, made by an admin on the **API tokens** page (`/admin/tokens`): name it, copy the token (shown once, then
only its SHA-256 is kept), revoke it there when done. Send it on every request:

    Authorization: Bearer scat_<64 hex digits>

A token acts as the admin who made it. On each request the account is read from sc-identity: a disabled account, or one
that lost the `admin` role, is refused at once. There are no cookies and no CSRF tokens, so a browser session is not
accepted. On a TLS-only site (`SCBACKEND_SECURE_COOKIES`) a request whose `X-Forwarded-Proto` is not `https` is refused
with 403 before its token is read. At most 20 live tokens per account. Tokens do not expire; revoke them.

Tokens are meant to be replaced later by something that can be scoped and expire (service identities); the page and the
`Bearer` header are the contract for now.

## Conventions

- Bodies are JSON objects; answers are JSON. Errors are `{"error": "..."}`.
- Statuses: 400 a bad request or a refused value, 401 no or an invalid token (`WWW-Authenticate: Bearer`), 403 not an
  admin or plain HTTP, 404 an unknown account, class or player, 409 the player is on a shard or an owner refused, 429
  throttled, 502/503/504 an owner is not answering.
- An action takes an optional `request_id` (8 to 64 letters, digits, `-`, `_`; one is made when absent). Repeating it
  applies the action once. It is echoed back.
- An action answers `{"ok": true, "message": "...", "request_id": "...", "result": <the audit's "after">}`.
- Every action needs a `reason` (1 to 500 characters) and, for changes to what a player wears or holds, the player off any shard (adding items is allowed while they play).

## Endpoints

| Method and path | What |
|---|---|
| `GET /accounts?q=&limit=&page=` | Search (handle, email or id): `{accounts: [...], next_page}`. An account: `id, handle, display_name, email, roles, created_at_ms, disabled, player, badges` |
| `GET /accounts/{id}` | One account, with `now`: `offline`, `menu`, `playing` (with `shard`, `location`) or `unknown` |
| `GET /accounts/{id}/inventory` | `{player, now, inventories: [{id, parent, depth, live, item_count, items: [...]}], equipment: {worn, ports}}` |
| `GET /inventory/classes?q=&cat=&opt=` | Item class search, as the catalog on the inventory page: `{classes: [{guid, name, class, type, sub, room, container, capacity, stackLimit}]}` |
| `POST /accounts/{id}/inventory/add-batch` | `{inventory, items: [{class, amount?, copies?}], reason}`; `items` may also be the page's `guid:amount:copies,...` text. `result.items` has the new GEIDs |
| `POST /accounts/{id}/inventory/equip` | `{item, port: "<worn item geid>:<port>", reason}` (or `parent` and `port` apart) |
| `POST /accounts/{id}/inventory/unequip` | `{item, to: "<inventory id>", reason}` |
| `GET /accounts/{id}/badges` | `{known: [{id, name, developer}], held: [ids]}` |
| `PUT /accounts/{id}/badges` | `{badges: [ids]}`: the whole set the account will hold |
| `POST /accounts/{id}/names` (or `PUT`) | `{handle?, display_name?}`: either or both, a field left out stays. The handle is 3 to 32 of A-Z, a-z, 0-9, `_` and `-`, unique whatever the case (409 when taken); the name is 1 to 32 characters without control characters, empty goes back to the handle. 409 while the player is on a shard. Audited as `account.names`. The game shows the change from the player's next login |
| `GET /accounts/{id}/ships` | The ships in the player's hangars: `{player, ships: [{geid, class_guid, name, state, pad_size, location, insurance, entitlement_urn, created_at_ms}], repaired, orphans}` |
| `POST /accounts/{id}/ships/grant` | `{ships, location, insurance?, days?, pad?, reason}`: `ships` is `[{class, copies?}]` or the page's `guid:copies,...` text; `location` a hangar location id; `insurance` `none`, `lifetime` or `duration` (with `days`). Each ship gets a delivered entitlement and is stored in the hangar. 409 while the player is on a shard. Audited as `ship.grant` |
| `POST /accounts/{id}/ships/remove` | `{ship: <geid>, reason}`: removes the ship for good (its record, entity with its loadout and row in sc-entitygraph) and revokes its entitlement. 409 while the ship is deployed or the player on a shard; a repeat of the request answers `result.already_removed`. Audited as `ship.remove` with the ship as `before` |
| `GET /audit?limit=` | The newest audit records (default 100, at most 500) |
| `GET /accounts/{id}/audit?limit=` | The same for one account |

Moving and removing items, money, assets, putting ships away and location are not in the API yet; they stay on the pages.

## Example

    curl -H "Authorization: Bearer $SC_TOKEN" "$SITE/admin/api/v1/accounts?q=pilot"
    curl -H "Authorization: Bearer $SC_TOKEN" -d '{"inventory":"300000000002:Location:2026442305","reason":"test","items":[{"class":"<guid>","amount":5}]}' \
      "$SITE/admin/api/v1/accounts/1000002/inventory/add-batch"
