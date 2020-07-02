//! Trait which allow others parts of rust-lightning to manage CPFP candidates
//! utxos for increasing feerate of time-sensitive transactions.


use bitcoin::blockdata::transaction::OutPoint as BitcoinOutPoint;
use bitcoin::blockdata::transaction::Transaction;

use ln::onchain_utils::BumpingOutput;

/// A trait which sould be implemented to provide fresh CPFP utxo for onchain
/// transactions.
///
/// Implementation MUST provision and bookmarked utxo correctly to ensure LN
/// channel security in case of adversarial counterparty or unfavorable mempool
/// congestion.
pub trait UtxoPool: Sync + Send {
	//XXX (follow-up) what level of reserse we should keep ?
	/// Provides fee value which must be reserved with regards to a new channel
	/// creation.
	fn map_utxo(&self, channel_provision: u64);
	//XXX: document better
	/// Allocate a utxo to cover fee required to confirm a pending onchain transaction.
	fn allocate_utxo(&self, required_fee: u64) -> Option<(BitcoinOutPoint, BumpingOutput)>;
	//XXX: document better
	/// Free a utxo. Call in case of reorg or counterparty claiming the output first.
	fn free_utxo(&self, free_utxo: BitcoinOutPoint);
	//XXX: document better
	/// Sign an allocated utxo as integrated by a CPFP.
	fn sign_utxo(&self, cpfp_transaction: &mut Transaction, utxo_index: u32);
}
