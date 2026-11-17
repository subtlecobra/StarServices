//! Ledger names and opening balances. The client asks for a currency by these exact names
//! (CWalletManager::AsyncGetCurrencyBalance_Client) and reads the first ledger of the reply.

pub const AUEC_LEDGER: &str = "AUEC_LEDGER";
pub const REC_LEDGER: &str = "REC_LEDGER";
pub const OPENING_AUEC: i64 = 20_000;
/// The game data defines no opening balance for REC, so this one is a server setting.
pub const OPENING_REC: i64 = 20_000;
