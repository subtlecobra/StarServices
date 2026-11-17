//! sc.external.services.solar_system_shop.v1.SolarSystemShopService. The same to every player, so no
//! caller is needed. An item id is an entity class GUID, which is how sc-eatransaction finds its price.
//!
//! This service owns `shop_inventory` and is its only seeder, through `SolarSystemShopInternal`, which sc-seeder and
//! `sc-admin seed` call with a seed bundle's rows: the items, which `sc-admin seed-export` reads from the game data, in
//! `SeedShopItems`, and the rental prices, which the game data lacks, in `SeedRentalPrices`.

use std::{net::SocketAddr, process::ExitCode, sync::Arc};

use clap::Parser;
use sc_db::{internal, DatabaseArgs, Postgres, Row};
use sc_api::internal::{ solar_system_shop_internal_server::{SolarSystemShopInternal, SolarSystemShopInternalServer}, RentalPrice, SeedRentalPricesRequest, SeedResponse, SeedShopItemsRequest, ShopItem, };
use sc_proto::{
    services::solar_system_shop::v1::{
    solar_system_shop_service_server::{SolarSystemShopService, SolarSystemShopServiceServer},
    GetGlobalInventoryRequest, GetGlobalInventoryResponse, InventoryItem, RentalOffering, ShopLoadout,
    },
};
use sc_runtime::{CommonArgs, MAX_MESSAGE_BYTES};
use serde_json::Value;
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

/// Unchanged from when this was shared, so an existing table is left as it is. Plain columns and JSON.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS shop_inventory (
    id TEXT PRIMARY KEY,
    buy_price DOUBLE PRECISION NOT NULL DEFAULT 0,
    sell_price DOUBLE PRECISION NOT NULL DEFAULT 0,
    current_inventory INT NOT NULL DEFAULT 0,
    max_inventory INT NOT NULL DEFAULT 0,
    gold_licenses_required BOOLEAN NOT NULL DEFAULT FALSE,
    platinum_licenses_required BOOLEAN NOT NULL DEFAULT FALSE,
    rental_offerings JSONB NOT NULL DEFAULT '[]',
    hidden_perk_item BOOLEAN NOT NULL DEFAULT FALSE,
    licence_perk_item BOOLEAN NOT NULL DEFAULT FALSE,
    available_box_sizes INT NOT NULL DEFAULT 0,
    shop_loadouts JSONB NOT NULL DEFAULT '[]'
);";
/// Rows per statement when seeding.
const SEED_ROWS: usize = 500;

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
}

struct Shop {
    db: Postgres,
}

fn entries(json: &str) -> Result<Vec<Value>, Status> {
    serde_json::from_str(json).map_err(internal)
}

fn rental_offerings(json: &str) -> Result<Vec<RentalOffering>, Status> {
    Ok(entries(json)?
        .iter()
        .map(|offering| RentalOffering {
            days: offering["days"].as_u64().unwrap_or(0) as u32,
            price: offering["price"].as_f64().unwrap_or(0.0),
        })
        .collect())
}

fn shop_loadouts(json: &str) -> Result<Vec<ShopLoadout>, Status> {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    Ok(entries(json)?
        .iter()
        .map(|loadout| ShopLoadout { id: text(&loadout["id"]), name: text(&loadout["name"]) })
        .collect())
}

fn item(row: &Row) -> Result<InventoryItem, Status> {
    Ok(InventoryItem {
        id: row.get("id"),
        buy_price: row.get("buy_price"),
        sell_price: row.get("sell_price"),
        current_inventory: row.get::<_, i32>("current_inventory") as u32,
        max_inventory: row.get::<_, i32>("max_inventory") as u32,
        gold_licenses_required: row.get("gold_licenses_required"),
        platinum_licenses_required: row.get("platinum_licenses_required"),
        rental_offerings: rental_offerings(row.get("offerings"))?,
        hidden_perk_item: row.get("hidden_perk_item"),
        licence_perk_item: row.get("licence_perk_item"),
        available_box_sizes: row.get::<_, i32>("available_box_sizes") as u32,
        shop_loadouts: shop_loadouts(row.get("loadouts"))?,
    })
}

#[tonic::async_trait]
impl SolarSystemShopService for Shop {
    async fn get_global_inventory(
        &self,
        _request: Request<GetGlobalInventoryRequest>,
    ) -> Result<Response<GetGlobalInventoryResponse>, Status> {
        let rows = self
            .db
            .query(
                "SELECT id, buy_price, sell_price, current_inventory, max_inventory, gold_licenses_required,
                        platinum_licenses_required, rental_offerings::text AS offerings, hidden_perk_item,
                        licence_perk_item, available_box_sizes, shop_loadouts::text AS loadouts
                 FROM shop_inventory ORDER BY id",
                &[],
            )
            .await
            .map_err(internal)?;
        let inventory_items = rows.iter().map(item).collect::<Result<_, _>>()?;
        Ok(Response::new(GetGlobalInventoryResponse { inventory_items }))
    }
}

/// `(class guid, offerings as JSON)`, refusing a price list that would break a rental.
fn rental_rows(prices: &[RentalPrice]) -> Result<Vec<(String, String)>, Status> {
    prices
        .iter()
        .map(|price| {
            if price.id.is_empty() {
                return Err(Status::invalid_argument("rental price with no id"));
            }
            if price.offerings.is_empty() {
                return Err(Status::invalid_argument(format!("{} has no offerings", price.id)));
            }
            let valid = |offering: &RentalOffering| offering.days > 0 && offering.price.is_finite() && offering.price >= 0.0;
            if !price.offerings.iter().all(valid) {
                return Err(Status::invalid_argument(format!("{} needs positive days and a price", price.id)));
            }
            let offerings: Vec<Value> = price
                .offerings
                .iter()
                .map(|offering| serde_json::json!({ "days": offering.days, "price": offering.price }))
                .collect();
            Ok((price.id.clone(), Value::from(offerings).to_string()))
        })
        .collect()
}

/// Sets the rental offerings of the listed items, adding those the shop does not have yet.
async fn seed_rental_prices(db: &Postgres, prices: &[RentalPrice]) -> Result<u64, Status> {
    let rows = rental_rows(prices)?;
    let mut written = 0;
    for chunk in rows.chunks(SEED_ROWS) {
        let ids: Vec<&str> = chunk.iter().map(|row| row.0.as_str()).collect();
        let offerings: Vec<&str> = chunk.iter().map(|row| row.1.as_str()).collect();
        written += db
            .execute(
                "INSERT INTO shop_inventory (id, rental_offerings)
                 SELECT id, offerings::jsonb FROM unnest($1::text[], $2::text[]) AS rows (id, offerings)
                 ON CONFLICT (id) DO UPDATE SET rental_offerings = excluded.rental_offerings",
                &[&ids, &offerings],
            )
            .await
            .map_err(internal)?;
    }
    Ok(written)
}

/// Adds the items the shop does not have yet. Rows an operator has changed keep their prices, because only new ids
/// are added.
async fn seed_shop_items(db: &Postgres, items: &[ShopItem]) -> Result<u64, Status> {
    if items.iter().any(|item| item.id.is_empty()) {
        return Err(Status::invalid_argument("shop item with no id"));
    }
    let mut written = 0;
    for chunk in items.chunks(SEED_ROWS) {
        let ids: Vec<&str> = chunk.iter().map(|item| item.id.as_str()).collect();
        let buy: Vec<f64> = chunk.iter().map(|item| item.buy_price).collect();
        let sell: Vec<f64> = chunk.iter().map(|item| item.sell_price).collect();
        let current: Vec<i32> = chunk.iter().map(|item| item.current_inventory).collect();
        let max: Vec<i32> = chunk.iter().map(|item| item.max_inventory).collect();
        written += db
            .execute(
                "INSERT INTO shop_inventory (id, buy_price, sell_price, current_inventory, max_inventory)
                 SELECT * FROM unnest($1::text[], $2::float8[], $3::float8[], $4::int4[], $5::int4[])
                 ON CONFLICT DO NOTHING",
                &[&ids, &buy, &sell, &current, &max],
            )
            .await
            .map_err(internal)?;
    }
    Ok(written)
}

#[tonic::async_trait]
impl SolarSystemShopInternal for Shop {
    async fn seed_shop_items(&self, request: Request<SeedShopItemsRequest>) -> Result<Response<SeedResponse>, Status> {
        let rows = seed_shop_items(&self.db, &request.into_inner().items).await?;
        info!(rows, "shop items seeded");
        Ok(Response::new(SeedResponse { rows }))
    }

    async fn seed_rental_prices(
        &self,
        request: Request<SeedRentalPricesRequest>,
    ) -> Result<Response<SeedResponse>, Status> {
        let rows = seed_rental_prices(&self.db, &request.into_inner().prices).await?;
        info!(rows, "rental prices seeded");
        Ok(Response::new(SeedResponse { rows }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let db = Postgres::connect(&args.database.postgres, SCHEMA).await?;
    let service = Arc::new(Shop { db });
    let server = SolarSystemShopServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    // On the same port: the gateway routes by service name, and no GRPCRoute lists the internal one.
    let internal = SolarSystemShopInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "solar system shop listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_columns_become_messages() {
        let offerings = rental_offerings(r#"[{"days": 3, "price": 12.5}]"#).unwrap();
        assert_eq!(offerings, [RentalOffering { days: 3, price: 12.5 }]);
        let loadouts = shop_loadouts(r#"[{"id": "a", "name": "b"}]"#).unwrap();
        assert_eq!(loadouts, [ShopLoadout { id: "a".into(), name: "b".into() }]);
        assert!(rental_offerings("[]").unwrap().is_empty());
        assert!(rental_offerings("{").is_err());
    }

    fn price(id: &str, days: u32, price: f64) -> RentalPrice {
        RentalPrice { id: id.into(), offerings: vec![RentalOffering { days, price }] }
    }

    #[test]
    fn rental_prices_are_stored_as_the_shop_reads_them() {
        let rows = rental_rows(&[price("abc", 1, 100.0)]).unwrap();
        assert_eq!(rows, [("abc".to_string(), r#"[{"days":1,"price":100.0}]"#.to_string())]);
        assert_eq!(rental_offerings(&rows[0].1).unwrap(), [RentalOffering { days: 1, price: 100.0 }]);
    }

    #[test]
    fn rental_prices_that_would_break_a_rental_are_refused() {
        for prices in [
            price("", 1, 1.0),
            RentalPrice { id: "a".into(), offerings: Vec::new() },
            price("a", 0, 1.0),
            price("a", 1, -1.0),
            price("a", 1, f64::NAN),
        ] {
            assert!(rental_rows(&[prices.clone()]).is_err(), "{prices:?}");
        }
    }
}
