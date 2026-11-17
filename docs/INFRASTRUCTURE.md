# Infrastructure

The deployment as it runs on the cluster (read from the cluster and from `README.md` and `DATA.md`, 2026-11-16).
Ownership of data and the gRPC calls between services are listed in [DATA.md](DATA.md).

```mermaid
%%{init: {"theme":"base","themeVariables":{"fontSize":"16px","fontFamily":"Helvetica, Arial, sans-serif"},"flowchart":{"nodeSpacing":40,"rankSpacing":60}}}%%
flowchart TB
    classDef user fill:#fff3cd,stroke:#b8860b,color:#000
    classDef edge fill:#d6eaf8,stroke:#2874a6,color:#000
    classDef svc fill:#e8f6e9,stroke:#2e7d32,color:#000
    classDef game fill:#fde2e2,stroke:#c0392b,color:#000
    classDef store fill:#ede7f6,stroke:#5e35b1,color:#000
    classDef ops fill:#f0f0f0,stroke:#555,color:#000

    subgraph L1["1. Players"]
        game["🎮 Star Citizen game"]
        browser["🌐 Web browser<br/>(account portal, admin)"]
    end

    subgraph L2["2. Ways in"]
        envoy["Envoy Gateway<br/>all gRPC and web traffic"]
        hub["sc-hub<br/>login and join routing"]
        fe["replicant-server<br/>the live game connection (UDP)"]
    end

    subgraph L3["3. Backend services (one program each)"]
        accounts["👤 Accounts and sessions<br/>login, identity, portal, admin"]
        social["💬 Social<br/>friends, chat, squads, presence"]
        economy["💰 Economy<br/>ledger, entitlements, rentals, deliveries"]
        gameplay["🧩 Player and game state<br/>customizer, documents, reputation,<br/>missions, stats"]
    end

    subgraph L4["4. The simulated world, one set per shard"]
        dgs["sc-dgs servers<br/>each runs one area of space:<br/>Stanton ×7, Pyro ×2, Nyx ×1"]
        egraph["sc-entitygraph<br/>who owns and contains what"]
        gd["sc-gamedata<br/>game files of each build"]
    end

    subgraph L5["5. Storage"]
        yb[("YugabyteDB<br/>accounts, money,<br/>documents")]
        arango[("ArangoDB<br/>entities and ships")]
        valkey[("Valkey<br/>game data cache")]
    end

    subgraph L6["Operations"]
        op["sc-shard-operator<br/>opens and closes shards"]
        reg[("Image registry<br/>publish.sh pushes here")]
    end

    game -->|"login, menus"| envoy
    game -->|"pick a server"| hub
    game ==>|"play"| fe
    browser --> envoy

    envoy --> accounts & social & economy & gameplay
    hub -->|"needs a shard"| op
    fe <-->|"world updates"| dgs

    dgs -->|"items, ships"| egraph
    dgs -->|"files"| gd
    dgs -->|"money, appearance"| economy
    dgs -->|"money, appearance"| gameplay

    accounts & social & economy & gameplay --> yb
    egraph --> arango
    gd --> valkey

    op -.->|"creates"| dgs
    reg -.->|"images"| op

    class game,browser user
    class envoy,hub,fe edge
    class accounts,social,economy,gameplay svc
    class dgs game
    class egraph,gd svc
    class yb,arango,valkey store
    class op,reg ops
```

## Reading it

- **Entry points**: the game reaches the gRPC services only through the Envoy Gateway `sc-edge`; `sc-hub` and the
  shard's `replicant-server` use NodePorts (the game's TCP and UDP traffic is not proxied).
- **Shards**: each shard is its own namespace holding one `replicant-server` (`sc-pu-frontend`) and one `sc-dgs` pod per
  area. Shard namespaces are made and removed by `sc-shard-operator`; a DGS has no database access and asks the owning
  services over gRPC.
- **Data**: relational data is in YugabyteDB (`scbackend-data`), entities in each region's ArangoDB, game data in each
  region's Valkey behind `sc-gamedata`. Only the owning service touches its tables or collections.
- **Deployment**: this repository only publishes to the registry; a separate deployment pulls from it. Arrows from the
  registry are the pulls.
- Edges are the main ones, not every call: [DATA.md](DATA.md) has the full table.
