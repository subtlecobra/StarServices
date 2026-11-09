# Portal and web accounts

`sc-portal` is the public website and JSON API where players register, log in, manage their account and get
the files the game needs to log in. The admin pages are `sc-admin-web` (`services/admin`), under `/admin` on
the same site. Neither keeps data. Accounts, passwords, roles, web sessions and the login audit belong to
`sc-identity`, which they ask over `IdentityInternal` (see the "Portal and admin" part of
`proto/internal/scbackend/internal/identity/v1/identity.proto`).

```
browser ── HTTPS ──> sc-edge (Envoy Gateway, the game's TLS listener)
                       ├─ HTTPRoute /, /register, /login, /logout, /forgot, /account, /play, /api, /assets ─> sc-portal:8080
                       └─ HTTPRoute /admin ─> sc-admin-web:8080
sc-portal, sc-admin-web ── gRPC IdentityInternal ──> sc-identity
```

The routes are in the `sc-edge` chart (`httpRoutes`). They list the sites' own paths and never the prefix `/`, so
no gRPC call (`/<package>.<Service>/<Method>`) can match them. The site is served on the game's address and
port (`https://127.0.0.2:50051/` on the dev cluster), with the certificate of the backend's own CA:
import `client.crt` into the browser, or accept the warning.

## Pages and API

| Path | What |
|---|---|
| `GET /` | Home |
| `GET, POST /register` | Handle, email, password. Logs in and goes to `/play` |
| `GET, POST /login` | Handle or email, and password |
| `POST /logout` | Ends the session |
| `GET /account` | Handle, email, roles, change password (`POST /account/password`), sessions with revoke (`POST /account/sessions/revoke`) |
| `GET /play` | How to play; `POST /play/login-data` downloads `LoginData.json`, `GET /play/client.crt` the CA |
| `GET /forgot` | Says to ask an admin: no mail sender exists |
| `/admin` | sc-admin-web, linked only for accounts with the `admin` role |

The JSON API takes and returns JSON, with errors as `{"error": "..."}`: `POST /api/register`, `POST /api/login`,
`POST /api/logout`, `GET /api/me` (the account and the CSRF token), `POST /api/password`, `GET /api/sessions`,
`POST /api/sessions/revoke`, `POST /api/game-login` (the content of `LoginData.json`).

## Playing with a new account

1. Register at `/register`. The account gets the next index from 100,001 on, and its account id and GEID follow
   from it as for the accounts sc-admin issues (1,000,000 and 300,000,000,000 plus the index), which is what the DGS
   derives the player's hangar and items from. sc-admin's own accounts count up from 1, so the two never meet.
2. On `/play`, download `LoginData.json` and `client.crt`. `LoginData.json` is what `sc-admin client-files` writes:
   the handle, a game token for this account signed by sc-identity (with the secret of `state.json`, valid 30 days),
   and the backend's addresses (`SCBACKEND_GAME_HOST`, the environment's `host`; ports 50051 and 8000).
3. Put both in a private game folder and its `Bin64`, and start the game from there with the arguments the page
   lists and `GRPC_DEFAULT_SSL_ROOTS_FILE_PATH` set to `client.crt`, as the private launcher does.

The private launcher (`sc-launch-private.sh` in the game prefix) still writes its own `LoginData.json` with
`sc-admin client-files` on every start, for `SC_HANDLE` from `state.json`. To play a portal account with it, it needs
an option to use a downloaded `LoginData.json` instead; see TODO.md. Do not run it with `SC_HANDLE` set to a portal
account's handle: sc-admin would issue a second, unrelated account by that name, which sc-identity then ignores.

## Admin pages

`sc-admin-web` serves `/admin`: account search, and per account the tabs Overview (disable, roles, password reset),
Money, Assets, Ships, Location, Inventory and Audit. Every change is a post with the CSRF token and a request id, is
written to `admin_audit` as `pending` before the owner is asked, and is finished with the outcome and the before and
after; the owner applies the key `admin:<request id>` once. The account's player is `AccountSummary.geid` from
sc-identity.

The outcome of a change is shown once, on the page the post redirects to. It is kept in the portal's memory, per admin, for
at most a minute, never in the address or a cookie, so a refreshed or copied link shows no stale notice; the next page
read takes it and it is gone.

The Inventory tab asks sc-entitygraph (`EntityGraphInternal`, the "Admin inventories" block, see
[INVENTORY.md](INVENTORY.md)):

- The tree of the player's inventories: personal, item banks by location, other player inventories, and the
  containers of what the player owns (ship holds, crates, backpacks) nested under the inventory they are in, with
  capacity and occupancy in SCU and each item's class, English name, GUID, stack, owner and the inventories it holds.
  An inventory held by something live on a shard, and an item on a shard, are shown without actions.
- Adding items is a catalog with a cart: type to filter the item classes as you go, or pick one of the categories
  (Weapons, Armor, Clothing, Utility, Ammo, Vehicle parts, Sustenance, Containers, Other), each a chip with an icon.
  The categories and what each one matches (item type and sub type pairs) follow the game's own inventory filters
  (DataCore, `PersonalThoughtInventoryCategoryFilter`); the labels are the portal's, and Other is every class no
  category matches. The chosen category shows its own sub-filters and no other's: an armor's part and weight (light,
  medium, heavy; the weight is the portal's addition), a weapon's kind, a clothing's part, and so on. Choices in a
  group are OR, groups are AND. Only classes with an English name are listed.
- Each card has a `+` button, next to an amount for items that stack (consumables, food and drink up to 99, currency
  up to 999; the portal's rule, as the game defines no per-class limit) and without one for items that do not. An
  item joins the cart only when `+` is clicked. The cart is given in one batch to the chosen inventory under one
  reason (`/inventory/add-batch`, one audit entry). The script is `inventory.js`, served by the portal (never inline,
  built from text nodes); without it the plain search form still works. The categories are in
  `services/admin/src/catalog.rs`.
- Move an item to another inventory of the tree, or part of its stack (a split makes a new item); add an item by
  entity class, found with the class search (class name, English name or GUID), into any inventory of the tree; remove
  an item and everything it holds. Each needs a reason.
- Where to put an item (the add form, each item's Move, and Take off) is picked in a tree shown on the page
  (`tree.js`, served by the portal, never inline, built from text nodes): a container sits under the place or item that
  holds it, branches expand and collapse (Expand all, Collapse all), the arrow keys walk it and Enter chooses; the choice
  is shown as a path (`Orison › Item bank › Pembroke Core`). Without the script the same select is a plain indented list.
  Each item says `In: <path>` and each inventory heading is its path.
- Character & storage: the worn gear and the inventories are one tree. The character is at the top, open, with what it
  wears under what it hangs from (body, undersuit, armor, weapon, by port); below it Storage: the personal inventory
  (open), the item banks and the other top-level inventories, each a collapsed branch. A container's inventory is nested
  under the item that holds it, so a crate or a backpack is a branch of its own, collapsed until opened. A branch says its
  name, an occupancy bar and words (`0.5 of 2 SCU`) and its item count; an item says its name, the quantity, its GEID and
  class (the class GUID on hover). An inventory whose holder is not among the loaded items of the inventory it sits in
  is listed under "Also inside" there.
- Each item row has a small Actions menu: Equip (only for an item that fits a free port of something worn, with those
  ports to choose from), Move..., and Remove for a stowed item; Take off (into the personal inventory by default) for
  a worn one. Move, and putting a worn item somewhere else, are on the item's own page (`/inventory/item?geid=`), which
  has the inventory tree picker, so no row carries a list of every inventory (the old page did, per item). A stack's
  part is moved with the quantity.
- Paging: the items of an inventory are 25 to a page, each inventory with its own pager (`pg:<inventory id>=<page>` in the
  query), and the top-level inventories 25 to a page (`ip=<page>`). The tree is read whole from sc-entitygraph (up to 500
  items per inventory); the paging is the portal's. A redirect after a change comes back with `open=<ids>` and the anchor
  of the inventory worked on, which opens it and the way down to it; `details.js` (served by the portal, text nodes only)
  also opens the sections on the way to any `#anchor` and adds Expand all / Collapse all. Without scripts the server has
  opened what matters.
- Refused, as locations are, while sc-hub has the player on a shard or does not answer: the DGS holds a playing
  player's items. sc-entitygraph checks the rest as for the player's own command: capacity, loops, ownership.

The Ships tab is one page of collapsible sections, each with its count: Ships in hangars (open), Grant ships (closed until
a search or a cart is open), Ship entitlements and Rentals. The old Hangar tab was merged into it: `/hangar` leads to
`/ships` with its query, and `/hangar/grant` and `/hangar/report` still post. The hangar ships come from
sc-entitygraph's `ShipsInternal` (the same endpoint as the inventory, no extra setting; see [SHIPS.md](SHIPS.md)).

- Ships in hangars: `ListShips` for the player at every location: class (English name), state, pad size, the hangar's
  place by name, insurance (none, lifetime, or until a date), the claim's ready time and cost, and the numbers of ships the
  list `repaired` in the graph and of `orphans` it found. A pad size "not set" means the class has no size in the seed.
- Grant ships: find vehicle classes (the search filters the item type `NOITEM_Vehicle`, an assumption of the game data's
  naming; "any class" searches all), press Add to fill the cart (kept in the query string, no script; up to 10 classes and
  20 ships), then choose the home hangar (the home locations table of the game build; the player's home location is
  preselected), the insurance (none, a number of days, or lifetime), optionally a pad size, and a reason. One post calls
  `AcquireShip` per ship (command id `admin:<request id>:<n>`, so a resubmitted form makes no second ship) and writes
  one audit entry (`ship.grant`). The pad size and the claim times come from the class's seed (`vehicle.maxBoundingBox`,
  `vehicle.insurance`); in a cluster not re-seeded with them the pad size must be picked in the form, else the result says
  the ships are listed for no hangar, and the claim times are 0.
- A deployed ship left behind (a shard that crashed) can be put away at its record's hangar or marked destroyed
  (`ReportShip`, audited as `ship.store` / `ship.destroy`). Other transitions (claims, retrieval) are made in game.
- Ship entitlements (the vehicle entitlements and every rental, with the vehicle state saved for each) and Rentals (end,
  extend) are what the Ships tab showed before.
- Refused, like the inventory, while the player is on a shard or sc-hub does not answer.

Speed: a page asks its owners together, not one after the other. The Ships page makes the hub and presence checks, the hangar
list, the rentals, both entitlement queries, the home location and every class name in one go (the page takes as long as the
slowest owner), names each class once (the names are remembered by the portal), and reads the saved states of the entitlements
together. A change checks only the hub (not presence) before it asks the owner. The grant calls `AcquireShip` for one ship
after the other on purpose: each takes the entity graph's write locks, so concurrent calls would only queue, and a failure then
says how many were granted. The cost of the calls on sc-entitygraph's side is in [SHIPS.md](SHIPS.md).

## Accounts and admins

- The first admin: register on the portal, then `scripts/identity-admin.sh grant-role <handle> admin`, which
  port-forwards sc-identity and runs `sc-admin grant-role`. `revoke-role` takes it back, and
  `scripts/identity-admin.sh set-password <handle>` (password on standard input) sets a password, for instance
  for an account sc-admin issued, which has none, or when no admin is left to reset one.
- Password resets are done by an admin (`SetAccountState.new_password`), which also ends the account's web
  sessions. Email verification and reset by email need a mail sender: `SCBACKEND_MAIL` chooses one in sc-portal
  (`mail.rs`), and the only one is `off`. Emails are stored, unverified.
- `SCBACKEND_REGISTRATION=closed` stops new registrations.

## Data in sc-identity

All additive, created at start next to `accounts`:

| Table | Holds |
|---|---|
| `account_credentials` | Per account: email (lower case, unique), `email_verified_at_ms` (unused until mail exists), argon2id PHC hash, password change time, `disabled` |
| `account_roles` | `(account_id, role)`; the only role is `admin` |
| `web_sessions` | Random id, SHA-256 of the token (never the token), account, created, last seen, expiry, revoked, address, browser |
| `account_audit` | Every registration, login, failed or throttled login, password change, game token, admin change, with address and browser |
| `registered_account_index` | Sequence of registered account indexes, from 100,001 |
| `accounts_handle_folded` | Unique index on `lower(handle)`: handles are unique whatever their case |

## Security

Covered:

- **Passwords**: argon2id (19 MiB, 2 passes, 1 lane by default; `SCBACKEND_ARGON2_*`), a random 16-byte salt per hash,
  parameters stored in the PHC string. A login with a hash made under other parameters rehashes it, so raising them
  upgrades every account as it logs in. At most 4 hashes at once, off the async threads.
- **Password policy**: 10 to 128 characters, at least 4 different ones, not containing the handle or the email's
  local part, and not in an embedded list of common passwords and their stems (`password123` is refused as
  `password`).
- **No enumeration**: a wrong password, an unknown handle or email, and an account with no password get the same
  answer, and an unknown login verifies a dummy hash so it takes as long. A taken handle and a taken email get one
  answer. A disabled account is only reported after the right password. Throttling counts login names, not accounts.
- **Rate limits**, in sc-identity's database so all replicas share them: per 15 minutes, 10 failed logins for one name
  from one address, 100 for one name from anywhere, 50 from one address; 5 registrations per address per hour;
  10 wrong current passwords per account. sc-portal also allows 30 posts per address per minute per replica.
- **Sessions**: 32 random bytes, stored only as SHA-256; 30 days at most, 7 days idle; revoked on logout, on
  password change (the others), on an admin's reset or disable; listed and revocable on the account page.
- **Cookies**: `sc_session` is `HttpOnly; Secure; SameSite=Lax; Path=/`. `sc_csrf` is `Secure; SameSite=Strict`, readable
  by scripts for the header.
- **CSRF**: double submit (`sc_csrf` cookie, `csrf` form field or `x-csrf-token` header) on every post, including
  login and register; a request whose `Origin` or `Sec-Fetch-Site` says another site is refused; the CSRF token is
  replaced at login. The game-login download is a post for that reason.
- **Headers**: a CSP with no script and no inline style, `frame-ancestors 'none'`, `nosniff`, `same-origin` referrer,
  HSTS, `no-store` on everything but the stylesheet.
- **Limits**: 16 KiB request bodies, 20 s per request, 10 s per call to sc-identity.
- **Audit** of every account event in `account_audit`, kept 180 days; ended sessions are deleted after 30 days.
- **Secrets**: the portal mounts only `ca.crt` (Secret `scbackend-public`), never `state.json`. Game tokens are
  signed in sc-identity.

Not covered:

- No email: no verification, no reset by email, no notice of a new login or a password change.
- Game tokens are signed JWTs checked without a lookup, so disabling an account or changing its password does not
  end a game token already issued; it runs out after 30 days. sc-identity refuses new ones for a disabled account.
- No second factor, no CAPTCHA. The per-name throttle lets someone lock a name out for 15 minutes from many
  addresses (100 failures).
- The client address is the last `X-Forwarded-For` entry (`SCBACKEND_TRUSTED_PROXY_HOPS=1`). With the kind NodePort
  in front of Envoy that may be a node address rather than the player's, so per-address limits then count everyone
  together; set `externalTrafficPolicy: Local` on the Envoy Service, or the hops, when it is exposed for real.
- The CSRF token is not bound to the session, so something that can set cookies for the site (a sibling subdomain)
  could plant one. `Origin` checking covers modern browsers.
- The portal's own rate limit is per replica, in memory.
- The TLS certificate is from the backend's private CA, which browsers do not trust by default.
- The audit has no viewer here; sc-admin-web may add one. Admin actions through `SetAccountState` are audited
  without the admin's name, which sc-admin-web records itself.

## Deploy order

1. `sc-identity`, which creates its new tables at start (additive: `CREATE ... IF NOT EXISTS` only), and
   `sc-entitygraph`: sc-admin-web reads the player from `AccountSummary.geid` and asks entitygraph's admin RPCs, which
   older ones lack (an older sc-identity answers GEID 0, which the site shows as no player).
2. `sc-portal` (the deployment makes it wait for sc-identity) and `sc-admin-web`.
3. `sc-edge`, for the HTTPRoutes, and the deployment, which also creates the `scbackend-public`
   Secret the portal mounts.
4. `scripts/identity-admin.sh grant-role <handle> admin` for the first admin.
