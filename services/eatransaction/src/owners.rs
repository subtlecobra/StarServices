//! The owners of a purchase's data, over gRPC.

use std::time::Duration;

use sc_economy::{command::Push, ledger::REC_LEDGER};
use sc_api::internal::{ entitlement_internal_client::EntitlementInternalClient, ledger_internal_client::LedgerInternalClient, rental_internal_client::RentalInternalClient, CreateRentalRequest, ExtendRentalRequest, ExtendRentalResponse, GetRentalRequest, GetRentalResponse, GrantPurchaseRequest, LedgerDebitRequest, LedgerDebitResponse, LedgerRefundRequest, LedgerRefundResponse, SetRentalExpiryRequest, };
use sc_proto::{
    services::{
    ledger::v1::{varied_funds::Variation, Currency, Funds, VariedFunds},
    solar_system_shop::v1::{
    solar_system_shop_service_client::SolarSystemShopServiceClient, GetGlobalInventoryRequest, RentalOffering,
    },
    },
};
use sc_runtime::MAX_MESSAGE_BYTES;
use tonic::{
    transport::{Channel, Endpoint},
    Status,
};

use crate::purchase::{Owners, Record};

const VARIED_FUNDS: &str = "sc.external.services.ledger.v1.VariedFunds";
/// A call that takes longer has an unknown outcome, which the purchase is built to survive.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Addresses {
    pub ledger: String,
    pub entitlement: String,
    pub rental: String,
    pub shop: String,
    pub push: String,
}

pub struct GrpcOwners {
    ledger: LedgerInternalClient<Channel>,
    entitlement: EntitlementInternalClient<Channel>,
    rental: RentalInternalClient<Channel>,
    shop: SolarSystemShopServiceClient<Channel>,
    push: Push,
}

fn channel(name: &str, address: &str) -> Result<Channel, String> {
    let endpoint = Endpoint::from_shared(address.to_string()).map_err(|e| format!("--{name}: {e}"))?;
    Ok(endpoint.timeout(CALL_TIMEOUT).connect_timeout(CALL_TIMEOUT).connect_lazy())
}

impl GrpcOwners {
    pub fn connect(addresses: &Addresses) -> Result<Self, String> {
        Ok(Self {
            ledger: LedgerInternalClient::new(channel("ledger", &addresses.ledger)?),
            entitlement: EntitlementInternalClient::new(channel("entitlement", &addresses.entitlement)?),
            rental: RentalInternalClient::new(channel("rental", &addresses.rental)?),
            shop: SolarSystemShopServiceClient::new(channel("shop", &addresses.shop)?)
                .max_decoding_message_size(MAX_MESSAGE_BYTES),
            push: Push::connect(&addresses.push)?,
        })
    }
}

#[tonic::async_trait]
impl Owners for GrpcOwners {
    async fn offerings(&self, entity_class_guid: &str) -> Result<Option<Vec<RentalOffering>>, Status> {
        let inventory = self.shop.clone().get_global_inventory(GetGlobalInventoryRequest::default()).await?;
        let item = inventory.into_inner().inventory_items.into_iter().find(|item| item.id == entity_class_guid);
        Ok(item.map(|item| item.rental_offerings))
    }

    async fn rental(&self, geid: u64, id: &str) -> Result<GetRentalResponse, Status> {
        let request = GetRentalRequest { id: id.to_string(), player_id: geid };
        Ok(self.rental.clone().get_rental(request).await?.into_inner())
    }

    async fn debit(&self, request: LedgerDebitRequest) -> Result<LedgerDebitResponse, Status> {
        Ok(self.ledger.clone().debit(request).await?.into_inner())
    }

    async fn refund(&self, request: LedgerRefundRequest) -> Result<LedgerRefundResponse, Status> {
        Ok(self.ledger.clone().refund(request).await?.into_inner())
    }

    async fn grant(&self, request: GrantPurchaseRequest) -> Result<String, Status> {
        Ok(self.entitlement.clone().grant_purchase(request).await?.into_inner().urn)
    }

    async fn create_rental(&self, request: CreateRentalRequest) -> Result<(), Status> {
        self.rental.clone().create_rental(request).await?;
        Ok(())
    }

    async fn extend_rental(&self, request: ExtendRentalRequest) -> Result<ExtendRentalResponse, Status> {
        Ok(self.rental.clone().extend_rental(request).await?.into_inner())
    }

    async fn set_rental_expiry(&self, request: SetRentalExpiryRequest) -> Result<(), Status> {
        self.entitlement.clone().set_rental_expiry(request).await?;
        Ok(())
    }

    /// The balance display follows these. Topics under player/<geid>/ reach the ledger subscription.
    async fn funds_changed(&self, record: &Record, amount: i64, version: u64, deposited: bool) {
        let funds = VariedFunds {
            funds: Some(Funds { amount: amount as u64, currency: Currency::Rec as i32 }),
            ledger_name: REC_LEDGER.to_string(),
            log_message: if record.renewal { "RenewEARental" } else { "EARent" }.to_string(),
            variation: if deposited { Variation::Deposited } else { Variation::Withdrew } as i32,
            version: version as u32,
        };
        let topic = format!("player/{}/ledger", record.geid);
        self.push.notify(record.account_id, topic, VARIED_FUNDS, &funds).await;
    }
}
