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

The Inventory tab asks sc-entitygraph (`EntityGraphInternal`, the "Admin inventories" block, see
[INVENTORY.md](INVENTORY.md)):

- The tree of the player's inventories: personal, item banks by location, other player inventories, and the
  containers of what the player owns (ship holds, crates, backpacks) nested under the inventory they are in, with
  capacity and occupancy in SCU and each item's class, English name, GUID, stack, owner and the inventories it holds.
  An inventory held by something live on a shard, and an item on a shard, are shown without actions.
- Move an item to another inventory of the tree, or part of its stack (a split makes a new item); add an item by
  entity class, found with the class search (class name, English name or GUID), into any inventory of the tree; remove
  an item and everything it holds. Each needs a reason.
- Refused, as locations are, while sc-hub has the player on a shard or does not answer: the DGS holds a playing
  player's items. sc-entitygraph checks the rest as for the player's own command: capacity, loops, ownership.

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
