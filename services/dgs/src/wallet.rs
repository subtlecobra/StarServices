//! The wallet of a joining player: the three replicated variables of the client's `CWallet` component, set from the
//! player's ledgers in `sc-ledger`, which owns the money.
//!
//! What the client does when it registers the wallet's UI: it registers `wallet.UEC`, `wallet.REC` and
//! `wallet.MER` as replicated variables of the component (`TSerializedVariable`, 8 bytes, an `int64` balance), and binds
//! the UI bindings `wallet.UECBalance`, `wallet.RECBalance` and `wallet.MERBalance` (what the mobiGlas and the shops
//! read) to the same variables. Only an entity with authority changes them (`CWallet::UpdateCurrencyBalanceValue`), so
//! the value of a client is the one its bind carries. The keys are the CRC-32 of the lowercased names
//! and the type is that of an `int64`, the FNV-1a 64 of `int64` folded to 32 bits, as the other types of
//! `player.rs`.
//!
//! The ledger is asked at each join. A balance that changes afterwards reaches the client at its next join: an update
//! needs the variable's absolute index, which is not known.

use sc_api::internal::{
    ledger_internal_client::LedgerInternalClient, LedgerBalance, LedgerBalancesRequest,
};
use tonic::transport::Channel;
use tracing::warn;

use crate::entity::Snapshot;

/// CRC-32 of `wallet.uec`, `wallet.rec` and `wallet.mer`.
pub const UEC_KEY: u32 = 0xfdd8_f545;
pub const REC_KEY: u32 = 0xf897_e3c0;
pub const MER_KEY: u32 = 0x855d_277f;
/// The `int64` serializer: the FNV-1a 64 of `int64`, folded to 32 bits.
pub const INT64_TYPE: u32 = 0xbb53_8bcb;
/// The names of the ledgers, those the client asks for and `sc-ledger` opens.
pub const AUEC_LEDGER: &str = "AUEC_LEDGER";
pub const REC_LEDGER: &str = "REC_LEDGER";
/// The ledger of `MER`, which the client names `MER_LEDGER` and `sc-ledger` does not open.
pub const MER_LEDGER: &str = "MER_LEDGER";

/// The three variables for the balances of the player's ledgers, in the order `wallet.UEC`, `wallet.REC`, `wallet.MER`.
/// A ledger the player has not opened holds nothing.
pub fn snapshots(balances: &[LedgerBalance]) -> Vec<Snapshot> {
    let amount = |ledger: &str| {
        balances
            .iter()
            .find(|balance| balance.ledger_name == ledger)
            .map_or(0, |balance| balance.amount)
    };
    [
        (UEC_KEY, AUEC_LEDGER),
        (REC_KEY, REC_LEDGER),
        (MER_KEY, MER_LEDGER),
    ]
    .into_iter()
    .map(|(key, ledger)| Snapshot {
        key,
        type_id: INT64_TYPE,
        value: amount(ledger).to_le_bytes().to_vec(),
    })
    .collect()
}

/// The wallet of `geid`, whose ledgers `account` opens at their opening balances when they are not open yet. Without the ledger, or when it cannot say, the player's wallet is empty: a join never fails for
/// it.
pub async fn load(
    ledger: &mut LedgerInternalClient<Channel>,
    geid: u64,
    account: u32,
) -> Vec<Snapshot> {
    match ledger
        .balances(LedgerBalancesRequest {
            owner_geid: geid,
            open_for_account_id: account,
        })
        .await
    {
        Ok(reply) => snapshots(&reply.into_inner().balances),
        Err(status) => {
            warn!(code = ?status.code(), message = status.message(), geid, "no balances from the ledger, the wallet is empty");
            snapshots(&[])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crc32(name: &str) -> u32 {
        !name.to_ascii_lowercase().bytes().fold(!0u32, |crc, byte| {
            (0..8).fold(crc ^ u32::from(byte), |crc, _| {
                if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                }
            })
        })
    }

    fn fnv_folded(name: &str) -> u32 {
        let fnv = name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
        });
        (fnv >> 32) as u32 ^ fnv as u32
    }

    fn balance(name: &str, amount: i64) -> LedgerBalance {
        LedgerBalance {
            ledger_name: name.into(),
            amount,
            currency: 3,
            version: 1,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn the_keys_are_the_crc32_of_the_lowercased_names_the_client_registers() {
        assert_eq!(
            [UEC_KEY, REC_KEY, MER_KEY],
            [
                crc32("wallet.UEC"),
                crc32("wallet.REC"),
                crc32("wallet.MER")
            ]
        );
    }

    #[test]
    fn the_type_is_that_of_an_int64() {
        assert_eq!(INT64_TYPE, fnv_folded("int64"));
        assert_eq!(
            fnv_folded("uint32"),
            0x5d4e_4bea,
            "the same fold as the types the client already accepts"
        );
    }

    #[test]
    fn each_balance_goes_to_its_own_variable_as_a_little_endian_int64() {
        let wallet = snapshots(&[balance(REC_LEDGER, 7), balance(AUEC_LEDGER, 20_000)]);
        let seen: Vec<_> = wallet
            .iter()
            .map(|snapshot| (snapshot.key, snapshot.type_id, snapshot.value.clone()))
            .collect();
        assert_eq!(
            seen,
            [
                (UEC_KEY, INT64_TYPE, 20_000i64.to_le_bytes().to_vec()),
                (REC_KEY, INT64_TYPE, 7i64.to_le_bytes().to_vec()),
                (MER_KEY, INT64_TYPE, 0i64.to_le_bytes().to_vec()),
            ]
        );
    }

    #[test]
    fn a_player_with_no_ledger_has_an_empty_wallet() {
        assert!(snapshots(&[])
            .iter()
            .all(|snapshot| snapshot.value == [0; 8]));
    }

    /// A ledger that holds 20000 aUEC for one player, at an address of its own.
    async fn ledger(owner: u64) -> LedgerInternalClient<Channel> {
        use sc_api::internal::{
            ledger_internal_server::{LedgerInternal, LedgerInternalServer},
            LedgerAdjustRequest, LedgerAdjustResponse, LedgerBalancesResponse, LedgerDebitRequest,
            LedgerDebitResponse, LedgerOperationsRequest, LedgerOperationsResponse,
            LedgerRefundRequest, LedgerRefundResponse,
        };
        use tonic::{
            transport::{server::TcpIncoming, Server},
            Request, Response, Status,
        };

        struct Holding(u64);
        #[tonic::async_trait]
        impl LedgerInternal for Holding {
            async fn debit(
                &self,
                _: Request<LedgerDebitRequest>,
            ) -> Result<Response<LedgerDebitResponse>, Status> {
                Err(Status::unimplemented("debit"))
            }
            async fn refund(
                &self,
                _: Request<LedgerRefundRequest>,
            ) -> Result<Response<LedgerRefundResponse>, Status> {
                Err(Status::unimplemented("refund"))
            }
            async fn balances(
                &self,
                request: Request<LedgerBalancesRequest>,
            ) -> Result<Response<LedgerBalancesResponse>, Status> {
                let balances = if request.into_inner().owner_geid == self.0 {
                    vec![balance(AUEC_LEDGER, 20_000)]
                } else {
                    Vec::new()
                };
                Ok(Response::new(LedgerBalancesResponse { balances }))
            }
            async fn adjust(
                &self,
                _: Request<LedgerAdjustRequest>,
            ) -> Result<Response<LedgerAdjustResponse>, Status> {
                Err(Status::unimplemented("adjust"))
            }
            async fn operations(
                &self,
                _: Request<LedgerOperationsRequest>,
            ) -> Result<Response<LedgerOperationsResponse>, Status> {
                Err(Status::unimplemented("operations"))
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(
            Server::builder()
                .add_service(LedgerInternalServer::new(Holding(owner)))
                .serve_with_incoming(TcpIncoming::from(listener)),
        );
        LedgerInternalClient::new(
            tonic::transport::Endpoint::from_shared(format!("http://{address}"))
                .unwrap()
                .connect_lazy(),
        )
    }

    #[tokio::test]
    async fn the_ledger_gives_the_balance_of_the_player_and_an_empty_wallet_to_others() {
        let mut client = ledger(300_000_000_007).await;
        assert_eq!(
            load(&mut client, 300_000_000_007, 7).await[0].value,
            20_000i64.to_le_bytes()
        );
        assert_eq!(
            load(&mut client, 300_000_000_008, 8).await[0].value,
            0i64.to_le_bytes()
        );
    }

    #[tokio::test]
    async fn a_ledger_that_is_not_there_gives_an_empty_wallet() {
        let mut client = LedgerInternalClient::new(
            tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
        );
        assert!(load(&mut client, 1, 1)
            .await
            .iter()
            .all(|snapshot| snapshot.value == [0; 8]));
    }
}
