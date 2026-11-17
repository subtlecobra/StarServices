# Legal notice

**StarServices is an independent, unofficial, community research project. It is not made, endorsed, sponsored or
approved by Cloud Imperium Games, Roberts Space Industries or any of their affiliates.**

*Star Citizen*, *Roberts Space Industries*, *RSI* and related names, logos and marks are trademarks or registered
trademarks of their respective owners. They appear in this repository only to say which software StarServices is meant
to work with (nominative use). StarServices is not a game, does not use the game's branding and is not a substitute for
it. Ship, manufacturer, location and other in-game names that appear in the code, tests and documentation are used only
to identify what the code refers to.

## What this repository contains

- Original source code written for this project (the services, their internal API, the deployment charts and the
  scripts), under `MIT OR Apache-2.0` (see `LICENSE-MIT` and `LICENSE-APACHE`).
- Descriptions of the messages a game client exchanges with a server, written in our own words for interoperability and
  research, and the code that implements them.
- Small values a compatible server needs in order to talk to a client (message numbers, hashes and keys of variables,
  flags). They are facts about the protocol, not game content.

## What this repository does not contain, and must never contain

- Game code, binaries, libraries, assets, data files or archives, or anything extracted or decompiled from them
  (including `.proto` files, class, variable or method name tables and database dumps taken from the game).
  `proto/sc/` and the game data tables are produced on the maintainer's own machine from a game installation they own,
  stay out of version control, and are not to be committed, published or shared.
- Network captures (`.pcap`, `.pcap.zst`), recordings, game logs, session or login tokens, account data, keys or
  certificates of a real service or user. The certificates and keys the services use are generated for this deployment
  and match no real service.
- Addresses, offsets or other notes taken from the game's executable. Tools that read the game files of an installation
  you own to produce local, uncommitted files (`tools/protodump`, `tools/gamedata-build`) are part of the repository; their
  output is not.
- Anything that bypasses, disables or interferes with the game's anti-cheat or copy protection, or that targets the
  official service.

## What StarServices does

StarServices is a server a game client can be pointed at, for a private, non-commercial environment. It answers the
client's requests with its own data and logic. It does not patch, inject into or read the memory of the game process,
does not modify the game's files, does not relay or alter traffic to or from the official service, and does not talk to
it.

The tools in `tools/` that produce local files from the game (`protodump`, `gamedata-build`) read the game's files from disk,
on the maintainer's own machine, from a copy they own. They never touch the running game process.

## Your responsibility

- Use StarServices only with software and accounts you are entitled to use, and only in ways that the game's Terms of
  Service, End User License Agreement and applicable law allow. Those terms may restrict or forbid running the game
  against a server other than the official one. Read them. Breaking them can get an account suspended.
- Do not use StarServices to cheat, to harm other players, to disrupt or overload any service, to gain access to data
  that is not yours, or to bypass any access control.
- Do not publish game files, game data, captures or data from the game or its services. They can contain other people's
  personal data and the operator's proprietary data.
- Do not run it as a commercial service, and do not sell or trade items, currency or accounts from it.

## No warranty, no advice

StarServices is provided "as is", without warranty of any kind (see the license texts). This notice is the maintainers'
good-faith position, not legal advice. If you redistribute it, or you receive a complaint from anyone, talk to a lawyer.
The authors accept no liability for how the software is used.

## Takedown and contact

If you are a rights holder and believe something in this repository should not be here, open an issue or contact the
maintainer, GitHub user `subtlecobra`. It will be reviewed promptly and removed or changed where it should be, before
any discussion.
