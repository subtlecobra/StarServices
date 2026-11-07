//! A purchase as a sequence of calls to the services that own its data, recorded in this service's own
//! journal so that it always ends in one of two ways: charged and granted, or neither.
//!
//! The order is: record the order, charge, then grant. Every call carries the same idempotency key, so
//! running a step again changes nothing, and the journal says which step comes next:
//!
//! - `Pending`: ordered. The debit may or may not have happened.
//! - `Charged`: the ledger confirmed the debit. From here the purchase only goes forward: the grant and
//!   the rental are retried, by the client's retry or by the resume loop, until they are done. They are
//!   validated before the charge, so they have no reason to be refused.
//! - `Cancelling`: the debit's outcome is unknown, so it is refunded by its key. A refund that finds no
//!   debit spends the key, so a debit still in flight cannot charge afterwards.
//! - `Done` and `Failed` are final. A refused debit (insufficient funds) fails with nothing charged.
//!
//! So no failure can leave a charge without its grant (`Charged` is completed), nor a grant without its
//! charge (nothing is granted before the ledger confirms).

use std::time::{SystemTime, UNIX_EPOCH};

use sc_proto::{
    internal::{
        CreateRentalRequest, ExtendRentalRequest, ExtendRentalResponse, GetRentalResponse, GrantPurchaseRequest,
        LedgerDebitRequest, LedgerDebitResponse, LedgerRefundRequest, LedgerRefundResponse, SetRentalExpiryRequest,
    },
    services::{entitlement::v2::EntitlementSourceType, solar_system_shop::v1::RentalOffering},
};
use sc_economy::ledger::REC_LEDGER;
use tonic::{Code, Status};
use tracing::{error, warn};

/// Far beyond any price, and well inside what an f32 and a BIGINT can both hold.
const MAX_PRICE: f64 = 1e12;
const DAY_MS: i64 = 86_400_000;

/// The services that own a purchase's data.
#[tonic::async_trait]
pub trait Owners: Send + Sync {
    /// The shop's rental offerings for a class, or None when it lists none.
    async fn offerings(&self, entity_class_guid: &str) -> Result<Option<Vec<RentalOffering>>, Status>;
    async fn rental(&self, geid: u64, id: &str) -> Result<GetRentalResponse, Status>;
    async fn debit(&self, request: LedgerDebitRequest) -> Result<LedgerDebitResponse, Status>;
    async fn refund(&self, request: LedgerRefundRequest) -> Result<LedgerRefundResponse, Status>;
    async fn grant(&self, request: GrantPurchaseRequest) -> Result<String, Status>;
    async fn create_rental(&self, request: CreateRentalRequest) -> Result<(), Status>;
    async fn extend_rental(&self, request: ExtendRentalRequest) -> Result<ExtendRentalResponse, Status>;
    async fn set_rental_expiry(&self, request: SetRentalExpiryRequest) -> Result<(), Status>;
    /// Tells the player's client that its balance changed, which the push service carries. Best effort.
    async fn funds_changed(&self, record: &Record, amount: i64, version: u64, deposited: bool);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Pending,
    Charged,
    Cancelling,
    Done,
    Failed,
}

impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Step::Pending => "pending",
            Step::Charged => "charged",
            Step::Cancelling => "cancelling",
            Step::Done => "done",
            Step::Failed => "failed",
        }
    }

    pub fn from_name(name: &str) -> Option<Step> {
        [Step::Pending, Step::Charged, Step::Cancelling, Step::Done, Step::Failed]
            .into_iter()
            .find(|step| step.name() == name)
    }
}

/// One purchase, as the journal keeps it.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub geid: u64,
    pub account_id: u32,
    pub command: String,
    pub step: Step,
    pub renewal: bool,
    /// Empty until the journal assigns one to a new rental.
    pub rental_id: String,
    pub entity_class_guid: String,
    pub days: u32,
    pub amount: i64,
    /// For a new rental, fixed when ordered, so a resumed grant and rental agree.
    pub expire_at_ms: i64,
    pub log_context: Vec<u8>,
    pub log_message: String,
    pub error: String,
}

impl Record {
    /// The key of every call this purchase makes.
    pub fn key(&self) -> String {
        format!("eatransaction:{}:{}", self.geid, self.command)
    }
}

/// This service's own record of purchases.
#[tonic::async_trait]
pub trait Journal: Send + Sync {
    async fn find(&self, geid: u64, command: &str) -> Result<Option<Record>, Status>;
    /// Records a new order, assigning a rental id when it has none. A command already recorded is
    /// returned as it stands.
    async fn open(&self, record: &Record) -> Result<Record, Status>;
    /// Moves `record` from its step to `to`, with its error. False when it had moved on already.
    async fn advance(&self, record: &Record, to: Step) -> Result<bool, Status>;
    /// Notes a failed attempt at a step, for the operator.
    async fn note(&self, record: &Record, error: &str) -> Result<(), Status>;
    /// Purchases not finished for a while, for the resume loop.
    async fn unfinished(&self, limit: i64) -> Result<Vec<Record>, Status>;
}

/// What a purchase came to.
#[derive(Debug, PartialEq)]
pub struct Receipt {
    pub rental_id: String,
    /// The amount and new ledger version, when this run made the charge.
    pub charged: Option<(i64, u64)>,
}

pub enum Purchase<'a> {
    Rent { entity_class_guid: &'a str },
    Renew { rental_id: &'a str },
}

pub struct Order<'a> {
    pub purchase: Purchase<'a>,
    pub price: f32,
    pub days: u32,
    pub log_context: Vec<u8>,
    pub log_message: String,
}

/// What a rental costs. The shop's offering decides when it lists the class. The client's price only
/// stands for a class the shop does not list.
pub fn amount_due(listed: Option<&[RentalOffering]>, price: f32, days: u32, listed_only: bool) -> Result<i64, Status> {
    if days == 0 {
        return Err(Status::invalid_argument("a rental needs a duration"));
    }
    // Rental shops in the game data list their items with no offerings at all.
    let listed = listed.filter(|offerings| !offerings.is_empty());
    if listed.is_none() && listed_only {
        return Err(Status::failed_precondition("this item has no listed rental price"));
    }
    let price = match listed {
        Some(offerings) => offerings
            .iter()
            .find(|offering| offering.days == days)
            .map(|offering| offering.price)
            .ok_or_else(|| Status::failed_precondition(format!("not offered for {days} days")))?,
        None if price > 0.0 => f64::from(price),
        None => return Err(Status::invalid_argument("invalid price")),
    };
    if !price.is_finite() || !(0.0..=MAX_PRICE).contains(&price) {
        return Err(Status::invalid_argument("invalid price"));
    }
    Ok(price.ceil() as i64)
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as i64)
}

/// A debit the ledger refused for good, as opposed to one whose outcome is unknown.
fn refused(status: &Status) -> bool {
    matches!(status.code(), Code::FailedPrecondition | Code::InvalidArgument | Code::NotFound | Code::PermissionDenied)
}

pub struct Purchases<O, J> {
    pub owners: O,
    pub journal: J,
    pub listed_prices_only: bool,
}

impl<O: Owners, J: Journal> Purchases<O, J> {
    /// Runs a command, or picks up where an earlier run of it stopped.
    pub async fn purchase(&self, geid: u64, account_id: u32, command: &str, order: &Order<'_>) -> Result<Receipt, Status> {
        if let Some(record) = self.journal.find(geid, command).await? {
            return self.drive(record).await;
        }
        let (renewal, rental_id, entity_class_guid) = match order.purchase {
            Purchase::Rent { entity_class_guid } => (false, String::new(), entity_class_guid.to_string()),
            Purchase::Renew { rental_id } => {
                let rental = self.owners.rental(geid, rental_id).await?;
                (true, rental_id.to_string(), rental.entity_class_guid)
            }
        };
        let offerings = self.owners.offerings(&entity_class_guid).await?;
        let amount = amount_due(offerings.as_deref(), order.price, order.days, self.listed_prices_only)?;
        let record = Record {
            geid,
            account_id,
            command: command.to_string(),
            step: Step::Pending,
            renewal,
            rental_id,
            entity_class_guid,
            days: order.days,
            amount,
            expire_at_ms: if renewal { 0 } else { now_ms() + i64::from(order.days) * DAY_MS },
            log_context: order.log_context.clone(),
            log_message: order.log_message.clone(),
            error: String::new(),
        };
        let record = self.journal.open(&record).await?;
        self.drive(record).await
    }

    /// Takes a purchase through its remaining steps.
    pub async fn drive(&self, mut record: Record) -> Result<Receipt, Status> {
        let mut charged = None;
        loop {
            let next = match record.step {
                Step::Done => return Ok(Receipt { rental_id: record.rental_id, charged }),
                Step::Failed => return Err(Status::failed_precondition(record.error)),
                Step::Pending => match self.owners.debit(self.debit_request(&record)).await {
                    Ok(debited) => {
                        if !debited.replayed {
                            charged = Some((record.amount, debited.version));
                            self.owners.funds_changed(&record, record.amount, debited.version, false).await;
                        }
                        Step::Charged
                    }
                    Err(status) if refused(&status) => {
                        record.error = status.message().to_string();
                        Step::Failed
                    }
                    Err(status) => {
                        warn!(key = %record.key(), %status, "debit outcome unknown, cancelling");
                        record.error = format!("cancelled, the ledger did not answer: {}", status.message());
                        Step::Cancelling
                    }
                },
                Step::Cancelling => {
                    let refund = LedgerRefundRequest { key: record.key(), owner_geid: record.geid };
                    let refund = self.owners.refund(refund).await.map_err(|status| {
                        warn!(key = %record.key(), %status, "refund failed, retried later");
                        Status::unavailable("the purchase is being cancelled")
                    })?;
                    if refund.refunded && refund.version > 0 {
                        self.owners.funds_changed(&record, refund.amount, refund.version, true).await;
                    }
                    if record.error.is_empty() {
                        record.error = "cancelled".to_string();
                    }
                    Step::Failed
                }
                Step::Charged => match self.fulfil(&record).await {
                    Ok(()) => Step::Done,
                    Err(status) => {
                        error!(key = %record.key(), %status, "charged purchase not granted yet, retried later");
                        self.journal.note(&record, status.message()).await?;
                        let message = format!("paid, and still being delivered: {}", status.message());
                        return Err(Status::unavailable(message));
                    }
                },
            };
            if self.journal.advance(&record, next).await? {
                record.step = next;
            } else {
                // Another run moved it on. Carry on from where that one left it.
                record = self
                    .journal
                    .find(record.geid, &record.command)
                    .await?
                    .ok_or_else(|| Status::internal("purchase vanished"))?;
            }
        }
    }

    fn debit_request(&self, record: &Record) -> LedgerDebitRequest {
        LedgerDebitRequest {
            key: record.key(),
            owner_geid: record.geid,
            account_id: record.account_id,
            ledger_name: REC_LEDGER.to_string(),
            amount: record.amount,
        }
    }

    /// The grant and the rental, each safe to repeat.
    async fn fulfil(&self, record: &Record) -> Result<(), Status> {
        if record.renewal {
            let extension = ExtendRentalRequest {
                key: record.key(),
                id: record.rental_id.clone(),
                player_id: record.geid,
                days: record.days,
                log_message: record.log_message.clone(),
            };
            let extended = self.owners.extend_rental(extension).await?;
            // A rental that ended took its entitlement with it, so the entity has to be made again.
            let expiry = SetRentalExpiryRequest {
                urn: extended.entitlement_urn,
                player_id: record.geid,
                rental_expiry_ms: extended.expire_at_ms,
                reactivate: true,
                log_message: record.log_message.clone(),
                command_id: record.command.clone(),
            };
            return self.owners.set_rental_expiry(expiry).await;
        }
        let grant = GrantPurchaseRequest {
            key: record.key(),
            account_id: record.account_id,
            player_id: record.geid,
            source_type: EntitlementSourceType::Rental as i32,
            entity_class_guid: record.entity_class_guid.clone(),
            rental_expiry_ms: record.expire_at_ms,
            log_message: record.log_message.clone(),
        };
        let urn = self.owners.grant(grant).await?;
        let rental = CreateRentalRequest {
            id: record.rental_id.clone(),
            account_id: record.account_id,
            player_id: record.geid,
            entitlement_urn: urn,
            entity_class_guid: record.entity_class_guid.clone(),
            days: record.days,
            expire_at_ms: record.expire_at_ms,
            log_message: record.log_message.clone(),
        };
        self.owners.create_rental(rental).await
    }

    /// Finishes what a crash or an unreachable owner left halfway. Returns how many it finished.
    pub async fn resume(&self) -> Result<usize, Status> {
        let mut finished = 0;
        for record in self.journal.unfinished(100).await? {
            let key = record.key();
            match self.drive(record).await {
                Ok(_) => finished += 1,
                Err(status) if status.code() == Code::FailedPrecondition => finished += 1,
                Err(status) => warn!(%key, %status, "purchase still unfinished"),
            }
        }
        Ok(finished)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
    };

    fn offering(days: u32, price: f64) -> RentalOffering {
        RentalOffering { days, price }
    }

    #[test]
    fn the_shop_price_beats_the_clients() {
        let offerings = [offering(3, 150.5), offering(7, 300.0)];
        assert_eq!(amount_due(Some(&offerings), 1.0, 7, false).unwrap(), 300);
        assert_eq!(amount_due(Some(&offerings), 1.0, 3, false).unwrap(), 151);
    }

    #[test]
    fn a_listed_class_needs_a_matching_offering() {
        let offerings = [offering(3, 150.5)];
        assert_eq!(amount_due(Some(&offerings), 1.0, 30, false).unwrap_err().code(), Code::FailedPrecondition);
        assert_eq!(amount_due(Some(&[]), 5.0, 1, false).unwrap(), 5);
        assert_eq!(amount_due(Some(&[]), 5.0, 1, true).unwrap_err().code(), Code::FailedPrecondition);
    }

    #[test]
    fn an_unlisted_class_takes_the_clients_price_rounded_up() {
        assert_eq!(amount_due(None, 99.25, 1, false).unwrap(), 100);
    }

    #[test]
    fn prices_that_could_mint_money_are_refused() {
        for price in [0.0, -5.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30] {
            assert_eq!(amount_due(None, price, 1, false).unwrap_err().code(), Code::InvalidArgument, "{price}");
        }
        assert_eq!(amount_due(None, 10.0, 0, false).unwrap_err().code(), Code::InvalidArgument);
        let absurd = [offering(1, 1e13)];
        assert_eq!(amount_due(Some(&absurd), 1.0, 1, false).unwrap_err().code(), Code::InvalidArgument);
    }

    #[test]
    fn steps_keep_their_names() {
        for step in [Step::Pending, Step::Charged, Step::Cancelling, Step::Done, Step::Failed] {
            assert_eq!(Step::from_name(step.name()), Some(step));
        }
        assert_eq!(Step::from_name("other"), None);
    }

    /// What a debit key came to in the fake ledger.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Operation {
        Debited(i64),
        Refunded(i64),
        Cancelled,
    }

    /// The owners, in memory, with failures to order. Each behaves as its real service does: keyed
    /// operations apply once.
    #[derive(Default)]
    pub struct FakeOwners {
        balance: Mutex<i64>,
        version: Mutex<u64>,
        operations: Mutex<HashMap<String, Operation>>,
        /// Entitlements by URN: (class, expiry, revoked).
        entitlements: Mutex<HashMap<String, (String, i64, bool)>>,
        /// Rentals by id: (urn, class, expiry, extensions applied).
        rentals: Mutex<HashMap<String, (String, String, i64, u32)>>,
        extensions: Mutex<HashMap<String, i64>>,
        offerings: Mutex<HashMap<String, Vec<RentalOffering>>>,
        /// Debits that apply, then lose their answer.
        lost_debits: AtomicUsize,
        refunds_down: AtomicUsize,
        rentals_down: AtomicUsize,
        grants_down: AtomicUsize,
        calls: Mutex<Vec<&'static str>>,
        /// Balance changes the client was told of, negative for a charge.
        notified: Mutex<Vec<i64>>,
    }

    impl FakeOwners {
        fn with_balance(balance: i64) -> Self {
            let owners = Self::default();
            *owners.balance.lock().unwrap() = balance;
            owners
        }

        fn balance(&self) -> i64 {
            *self.balance.lock().unwrap()
        }

        fn called(&self, name: &'static str) {
            self.calls.lock().unwrap().push(name);
        }

        fn take(counter: &AtomicUsize) -> bool {
            counter.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1)).is_ok()
        }
    }

    #[tonic::async_trait]
    impl Owners for FakeOwners {
        async fn offerings(&self, entity_class_guid: &str) -> Result<Option<Vec<RentalOffering>>, Status> {
            Ok(self.offerings.lock().unwrap().get(entity_class_guid).cloned())
        }

        async fn rental(&self, _geid: u64, id: &str) -> Result<GetRentalResponse, Status> {
            let rentals = self.rentals.lock().unwrap();
            let (urn, class, ..) = rentals.get(id).ok_or_else(|| Status::not_found("rental"))?;
            Ok(GetRentalResponse { entity_class_guid: class.clone(), entitlement_urn: urn.clone() })
        }

        async fn debit(&self, request: LedgerDebitRequest) -> Result<LedgerDebitResponse, Status> {
            self.called("debit");
            let mut operations = self.operations.lock().unwrap();
            let version = *self.version.lock().unwrap();
            match operations.get(&request.key) {
                Some(Operation::Debited(_)) => return Ok(LedgerDebitResponse { version, replayed: true }),
                Some(_) => return Err(Status::failed_precondition("cancelled")),
                None => {}
            }
            let mut balance = self.balance.lock().unwrap();
            if *balance < request.amount {
                return Err(Status::failed_precondition("insufficient funds"));
            }
            *balance -= request.amount;
            *self.version.lock().unwrap() += 1;
            operations.insert(request.key, Operation::Debited(request.amount));
            if Self::take(&self.lost_debits) {
                return Err(Status::deadline_exceeded("answer lost"));
            }
            Ok(LedgerDebitResponse { version: version + 1, replayed: false })
        }

        async fn refund(&self, request: LedgerRefundRequest) -> Result<LedgerRefundResponse, Status> {
            self.called("refund");
            if Self::take(&self.refunds_down) {
                return Err(Status::unavailable("ledger down"));
            }
            let mut operations = self.operations.lock().unwrap();
            match operations.get(&request.key).copied() {
                None => {
                    operations.insert(request.key, Operation::Cancelled);
                    Ok(LedgerRefundResponse { refunded: false, amount: 0, version: 0 })
                }
                Some(Operation::Cancelled) => Ok(LedgerRefundResponse::default()),
                Some(Operation::Refunded(amount)) => Ok(LedgerRefundResponse { refunded: true, amount, version: 0 }),
                Some(Operation::Debited(amount)) => {
                    *self.balance.lock().unwrap() += amount;
                    operations.insert(request.key, Operation::Refunded(amount));
                    let mut version = self.version.lock().unwrap();
                    *version += 1;
                    Ok(LedgerRefundResponse { refunded: true, amount, version: *version })
                }
            }
        }

        async fn grant(&self, request: GrantPurchaseRequest) -> Result<String, Status> {
            self.called("grant");
            if Self::take(&self.grants_down) {
                return Err(Status::unavailable("entitlement down"));
            }
            let urn = format!("urn:{}", request.key);
            let entry = (request.entity_class_guid, request.rental_expiry_ms, false);
            self.entitlements.lock().unwrap().entry(urn.clone()).or_insert(entry);
            Ok(urn)
        }

        async fn create_rental(&self, request: CreateRentalRequest) -> Result<(), Status> {
            self.called("create_rental");
            if Self::take(&self.rentals_down) {
                return Err(Status::unavailable("rental down"));
            }
            let entry = (request.entitlement_urn, request.entity_class_guid, request.expire_at_ms, 0);
            self.rentals.lock().unwrap().entry(request.id).or_insert(entry);
            Ok(())
        }

        async fn extend_rental(&self, request: ExtendRentalRequest) -> Result<ExtendRentalResponse, Status> {
            self.called("extend_rental");
            if Self::take(&self.rentals_down) {
                return Err(Status::unavailable("rental down"));
            }
            let mut rentals = self.rentals.lock().unwrap();
            let rental = rentals.get_mut(&request.id).ok_or_else(|| Status::not_found("rental"))?;
            let mut extensions = self.extensions.lock().unwrap();
            let expiry = *extensions.entry(request.key).or_insert_with(|| {
                rental.2 += i64::from(request.days) * DAY_MS;
                rental.3 += 1;
                rental.2
            });
            Ok(ExtendRentalResponse { expire_at_ms: expiry, entitlement_urn: rental.0.clone() })
        }

        async fn set_rental_expiry(&self, request: SetRentalExpiryRequest) -> Result<(), Status> {
            self.called("set_rental_expiry");
            let mut entitlements = self.entitlements.lock().unwrap();
            let entitlement = entitlements.get_mut(&request.urn).ok_or_else(|| Status::not_found("entitlement"))?;
            entitlement.1 = request.rental_expiry_ms;
            if request.reactivate {
                entitlement.2 = false;
            }
            Ok(())
        }

        async fn funds_changed(&self, _record: &Record, amount: i64, _version: u64, deposited: bool) {
            self.notified.lock().unwrap().push(if deposited { amount } else { -amount });
        }
    }

    #[derive(Default)]
    pub struct FakeJournal {
        records: Mutex<HashMap<(u64, String), Record>>,
        next_rental: AtomicUsize,
    }

    #[tonic::async_trait]
    impl Journal for FakeJournal {
        async fn find(&self, geid: u64, command: &str) -> Result<Option<Record>, Status> {
            Ok(self.records.lock().unwrap().get(&(geid, command.to_string())).cloned())
        }

        async fn open(&self, record: &Record) -> Result<Record, Status> {
            let mut record = record.clone();
            if record.rental_id.is_empty() {
                record.rental_id = format!("rental-{}", self.next_rental.fetch_add(1, Ordering::SeqCst));
            }
            let mut records = self.records.lock().unwrap();
            Ok(records.entry((record.geid, record.command.clone())).or_insert(record).clone())
        }

        async fn advance(&self, record: &Record, to: Step) -> Result<bool, Status> {
            let mut records = self.records.lock().unwrap();
            let stored = records.get_mut(&(record.geid, record.command.clone())).unwrap();
            if stored.step != record.step {
                return Ok(false);
            }
            stored.step = to;
            stored.error = record.error.clone();
            Ok(true)
        }

        async fn note(&self, record: &Record, error: &str) -> Result<(), Status> {
            let mut records = self.records.lock().unwrap();
            records.get_mut(&(record.geid, record.command.clone())).unwrap().error = error.to_string();
            Ok(())
        }

        async fn unfinished(&self, _limit: i64) -> Result<Vec<Record>, Status> {
            let records = self.records.lock().unwrap();
            let open = [Step::Pending, Step::Charged, Step::Cancelling];
            Ok(records.values().filter(|record| open.contains(&record.step)).cloned().collect())
        }
    }

    const GEID: u64 = 42;

    fn purchases(owners: FakeOwners) -> Purchases<FakeOwners, FakeJournal> {
        Purchases { owners, journal: FakeJournal::default(), listed_prices_only: false }
    }

    fn rent(class: &str, price: f32, days: u32) -> Order<'_> {
        Order {
            purchase: Purchase::Rent { entity_class_guid: class },
            price,
            days,
            log_context: Vec::new(),
            log_message: "EARent".into(),
        }
    }

    fn step(purchases: &Purchases<FakeOwners, FakeJournal>, command: &str) -> Step {
        purchases.journal.records.lock().unwrap()[&(GEID, command.to_string())].step
    }

    #[tokio::test]
    async fn a_rental_charges_grants_and_records_once() {
        let purchases = purchases(FakeOwners::with_balance(1000));
        purchases.owners.offerings.lock().unwrap().insert("ship".into(), vec![offering(3, 120.0)]);
        let receipt = purchases.purchase(GEID, 1, "c1", &rent("ship", 1.0, 3)).await.unwrap();
        assert_eq!(receipt.charged, Some((120, 1)));
        assert_eq!(purchases.owners.balance(), 880);
        let rentals = purchases.owners.rentals.lock().unwrap().clone();
        let (urn, class, expiry, _) = &rentals[&receipt.rental_id];
        assert_eq!(class, "ship");
        let entitlement = purchases.owners.entitlements.lock().unwrap()[urn].clone();
        assert_eq!(entitlement.1, *expiry, "the entitlement and the rental expire together");
        assert_eq!(*purchases.owners.calls.lock().unwrap(), ["debit", "grant", "create_rental"]);

        // The client's retry of the same command.
        let again = purchases.purchase(GEID, 1, "c1", &rent("ship", 1.0, 3)).await.unwrap();
        assert_eq!(again.rental_id, receipt.rental_id);
        assert_eq!(again.charged, None);
        assert_eq!(purchases.owners.balance(), 880);
        assert_eq!(purchases.owners.calls.lock().unwrap().len(), 3);
        assert_eq!(*purchases.owners.notified.lock().unwrap(), [-120], "the charge is shown once");
    }

    #[tokio::test]
    async fn insufficient_funds_grant_nothing() {
        let purchases = purchases(FakeOwners::with_balance(10));
        let refused = purchases.purchase(GEID, 1, "c1", &rent("ship", 50.0, 1)).await.unwrap_err();
        assert_eq!(refused.code(), Code::FailedPrecondition);
        assert_eq!(purchases.owners.balance(), 10);
        assert!(purchases.owners.entitlements.lock().unwrap().is_empty());
        assert_eq!(step(&purchases, "c1"), Step::Failed);
        let replayed = purchases.purchase(GEID, 1, "c1", &rent("ship", 50.0, 1)).await.unwrap_err();
        assert_eq!(replayed.message(), "insufficient funds");
    }

    /// The failure halfway: charged and granted, but the rental service is down. The purchase is kept
    /// as charged, and finishing it later grants nothing twice.
    #[tokio::test]
    async fn a_charge_whose_grant_fails_halfway_is_completed_not_lost() {
        let purchases = purchases(FakeOwners::with_balance(1000));
        purchases.owners.rentals_down.store(2, Ordering::SeqCst);
        let failed = purchases.purchase(GEID, 1, "c1", &rent("ship", 100.0, 2)).await.unwrap_err();
        assert_eq!(failed.code(), Code::Unavailable);
        assert_eq!(purchases.owners.balance(), 900);
        assert_eq!(step(&purchases, "c1"), Step::Charged);
        assert_eq!(purchases.owners.entitlements.lock().unwrap().len(), 1);
        assert!(purchases.owners.rentals.lock().unwrap().is_empty());

        // The resume loop, with the rental service still down, then back.
        assert_eq!(purchases.resume().await.unwrap(), 0);
        assert_eq!(purchases.resume().await.unwrap(), 1);
        assert_eq!(step(&purchases, "c1"), Step::Done);
        assert_eq!(purchases.owners.balance(), 900, "charged once");
        assert_eq!(purchases.owners.entitlements.lock().unwrap().len(), 1, "granted once");
        assert_eq!(purchases.owners.rentals.lock().unwrap().len(), 1);

        // The client's retry now finds it done.
        let receipt = purchases.purchase(GEID, 1, "c1", &rent("ship", 100.0, 2)).await.unwrap();
        assert!(purchases.owners.rentals.lock().unwrap().contains_key(&receipt.rental_id));
    }

    #[tokio::test]
    async fn an_entitlement_service_that_is_down_delays_the_grant_without_a_second_charge() {
        let purchases = purchases(FakeOwners::with_balance(1000));
        purchases.owners.grants_down.store(1, Ordering::SeqCst);
        let failed = purchases.purchase(GEID, 1, "c1", &rent("ship", 100.0, 2)).await.unwrap_err();
        assert_eq!(failed.code(), Code::Unavailable);
        let receipt = purchases.purchase(GEID, 1, "c1", &rent("ship", 100.0, 2)).await.unwrap();
        assert_eq!(receipt.charged, None, "the retry found the debit done");
        assert_eq!(purchases.owners.balance(), 900);
        assert_eq!(purchases.owners.entitlements.lock().unwrap().len(), 1);
    }

    /// The debit applied but its answer was lost: the purchase is cancelled and the money given back.
    #[tokio::test]
    async fn a_debit_with_no_answer_is_refunded_and_grants_nothing() {
        let purchases = purchases(FakeOwners::with_balance(1000));
        purchases.owners.lost_debits.store(1, Ordering::SeqCst);
        let failed = purchases.purchase(GEID, 1, "c1", &rent("ship", 100.0, 2)).await.unwrap_err();
        assert_eq!(failed.code(), Code::FailedPrecondition);
        assert_eq!(purchases.owners.balance(), 1000);
        assert!(purchases.owners.entitlements.lock().unwrap().is_empty());
        assert_eq!(step(&purchases, "c1"), Step::Failed);
        assert_eq!(*purchases.owners.notified.lock().unwrap(), [100], "the refund is shown");
    }

    #[tokio::test]
    async fn a_cancellation_that_cannot_reach_the_ledger_finishes_later() {
        let purchases = purchases(FakeOwners::with_balance(1000));
        purchases.owners.lost_debits.store(1, Ordering::SeqCst);
        purchases.owners.refunds_down.store(1, Ordering::SeqCst);
        let failed = purchases.purchase(GEID, 1, "c1", &rent("ship", 100.0, 2)).await.unwrap_err();
        assert_eq!(failed.code(), Code::Unavailable);
        assert_eq!(step(&purchases, "c1"), Step::Cancelling);
        assert_eq!(purchases.resume().await.unwrap(), 1);
        assert_eq!(purchases.owners.balance(), 1000);
        assert!(purchases.owners.entitlements.lock().unwrap().is_empty());
        // A retry cannot charge with the spent key.
        assert_eq!(
            purchases.purchase(GEID, 1, "c1", &rent("ship", 100.0, 2)).await.unwrap_err().code(),
            Code::FailedPrecondition
        );
        assert_eq!(purchases.owners.balance(), 1000);
    }

    #[tokio::test]
    async fn a_renewal_extends_the_rental_and_brings_its_entitlement_back() {
        let purchases = purchases(FakeOwners::with_balance(1000));
        purchases.owners.offerings.lock().unwrap().insert("ship".into(), vec![offering(1, 10.0), offering(2, 15.0)]);
        let rented = purchases.purchase(GEID, 1, "c1", &rent("ship", 1.0, 1)).await.unwrap();
        let urn = purchases.owners.rentals.lock().unwrap()[&rented.rental_id].0.clone();
        purchases.owners.entitlements.lock().unwrap().get_mut(&urn).unwrap().2 = true;
        let before = purchases.owners.rentals.lock().unwrap()[&rented.rental_id].2;

        let renew = Order {
            purchase: Purchase::Renew { rental_id: &rented.rental_id },
            price: 1.0,
            days: 2,
            log_context: Vec::new(),
            log_message: "RenewEARental".into(),
        };
        let renewed = purchases.purchase(GEID, 1, "c2", &renew).await.unwrap();
        assert_eq!(renewed.rental_id, rented.rental_id);
        assert_eq!(purchases.owners.balance(), 1000 - 10 - 15);
        let rental = purchases.owners.rentals.lock().unwrap()[&rented.rental_id].clone();
        assert_eq!(rental.2, before + 2 * DAY_MS);
        let entitlement = purchases.owners.entitlements.lock().unwrap()[&urn].clone();
        assert_eq!((entitlement.1, entitlement.2), (rental.2, false));

        purchases.purchase(GEID, 1, "c2", &renew).await.unwrap();
        assert_eq!(purchases.owners.rentals.lock().unwrap()[&rented.rental_id].3, 1, "extended once");
        let missing = Order { purchase: Purchase::Renew { rental_id: "none" }, ..renew };
        assert_eq!(purchases.purchase(GEID, 1, "c3", &missing).await.unwrap_err().code(), Code::NotFound);
        assert_eq!(purchases.owners.balance(), 1000 - 10 - 15);
    }

    #[tokio::test]
    async fn strict_pricing_refuses_before_charging() {
        let mut purchases = purchases(FakeOwners::with_balance(1000));
        purchases.listed_prices_only = true;
        let refused = purchases.purchase(GEID, 1, "c1", &rent("ship", 5.0, 1)).await.unwrap_err();
        assert_eq!(refused.code(), Code::FailedPrecondition);
        assert!(purchases.owners.calls.lock().unwrap().is_empty());
        assert!(purchases.journal.records.lock().unwrap().is_empty());
    }
}
