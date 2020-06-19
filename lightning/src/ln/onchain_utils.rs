//! Utilities for computing witnesses weight and feerate computation for onchain operation

use bitcoin::blockdata::transaction::{TxOut,TxIn, Transaction, SigHashType};
use bitcoin::blockdata::transaction::OutPoint as BitcoinOutPoint;
use bitcoin::blockdata::script::Script;

use bitcoin::hash_types::Txid;

use bitcoin::secp256k1::key::{SecretKey,PublicKey};

use ln::channelmanager::PaymentPreimage;
use ln::chan_utils::{TxCreationKeys, HTLCOutputInCommitment};
use ln::chan_utils;
use ln::msgs::DecodeError;
use ln::onchaintx::OnchainTxHandler;
use chain::chaininterface::{FeeEstimator, ConfirmationTarget, MIN_RELAY_FEE_SAT_PER_1000_WEIGHT};
use chain::keysinterface::ChannelKeys;
use util::byte_utils;
use util::logger::Logger;
use util::ser::{Readable, Writer, Writeable};

use std::collections::HashMap;
use std::cmp;
use std::ops::Deref;

const MAX_ALLOC_SIZE: usize = 64*1024;

#[derive(PartialEq, Clone, Copy)]
pub(crate) enum InputDescriptors {
	RevokedOfferedHTLC,
	RevokedReceivedHTLC,
	OfferedHTLC,
	ReceivedHTLC,
	RevokedOutput, // either a revoked to_local output on commitment tx, a revoked HTLC-Timeout output or a revoked HTLC-Success output
}

impl Writeable for InputDescriptors {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ::std::io::Error> {
		match self {
			&InputDescriptors::RevokedOfferedHTLC => {
				writer.write_all(&[0; 1])?;
			},
			&InputDescriptors::RevokedReceivedHTLC => {
				writer.write_all(&[1; 1])?;
			},
			&InputDescriptors::OfferedHTLC => {
				writer.write_all(&[2; 1])?;
			},
			&InputDescriptors::ReceivedHTLC => {
				writer.write_all(&[3; 1])?;
			}
			&InputDescriptors::RevokedOutput => {
				writer.write_all(&[4; 1])?;
			}
		}
		Ok(())
	}
}

impl Readable for InputDescriptors {
	fn read<R: ::std::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let input_descriptor = match <u8 as Readable>::read(reader)? {
			0 => {
				InputDescriptors::RevokedOfferedHTLC
			},
			1 => {
				InputDescriptors::RevokedReceivedHTLC
			},
			2 => {
				InputDescriptors::OfferedHTLC
			},
			3 => {
				InputDescriptors::ReceivedHTLC
			},
			4 => {
				InputDescriptors::RevokedOutput
			}
			_ => return Err(DecodeError::InvalidValue),
		};
		Ok(input_descriptor)
	}
}

pub(crate) fn get_witnesses_weight(inputs: &[InputDescriptors]) -> usize {
	let mut tx_weight = 2; // count segwit flags
	for inp in inputs {
		// We use expected weight (and not actual) as signatures and time lock delays may vary
		tx_weight +=  match inp {
			// number_of_witness_elements + sig_length + revocation_sig + pubkey_length + revocationpubkey + witness_script_length + witness_script
			&InputDescriptors::RevokedOfferedHTLC => {
				1 + 1 + 73 + 1 + 33 + 1 + 133
			},
			// number_of_witness_elements + sig_length + revocation_sig + pubkey_length + revocationpubkey + witness_script_length + witness_script
			&InputDescriptors::RevokedReceivedHTLC => {
				1 + 1 + 73 + 1 + 33 + 1 + 139
			},
			// number_of_witness_elements + sig_length + remotehtlc_sig  + preimage_length + preimage + witness_script_length + witness_script
			&InputDescriptors::OfferedHTLC => {
				1 + 1 + 73 + 1 + 32 + 1 + 133
			},
			// number_of_witness_elements + sig_length + revocation_sig + pubkey_length + revocationpubkey + witness_script_length + witness_script
			&InputDescriptors::ReceivedHTLC => {
				1 + 1 + 73 + 1 + 1 + 1 + 139
			},
			// number_of_witness_elements + sig_length + revocation_sig + true_length + op_true + witness_script_length + witness_script
			&InputDescriptors::RevokedOutput => {
				1 + 1 + 73 + 1 + 1 + 1 + 77
			},
		};
	}
	tx_weight
}

/// A struct to describe a revoked output, the templated witnessScript variables to claim it
/// (hash, timelock, pubkeys) and per_commitment_key to generate a solving witness. It is used by
/// OnchainTxHandler to generate a valid transaction claiming this output.
#[derive(Clone, PartialEq)]
pub(crate) struct RevokedOutput {
	per_commitment_point: PublicKey,
	remote_delayed_payment_base_key: PublicKey,
	remote_htlc_base_key: PublicKey,
	per_commitment_key: SecretKey,
	input_descriptor: InputDescriptors,
	amount: u64,
	htlc: Option<HTLCOutputInCommitment>,
	on_remote_tx_csv: u16,
}

impl Writeable for RevokedOutput {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ::std::io::Error> {
		self.per_commitment_point.write(writer)?;
		self.remote_delayed_payment_base_key.write(writer)?;
		self.remote_htlc_base_key.write(writer)?;
		writer.write_all(&self.per_commitment_key[..])?;
		self.input_descriptor.write(writer)?;
		writer.write_all(&byte_utils::be64_to_array(self.amount))?;
		self.htlc.write(writer)?;
		self.on_remote_tx_csv.write(writer)?;
		Ok(())
	}
}

impl Readable for RevokedOutput {
	fn read<R: ::std::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let per_commitment_point = Readable::read(reader)?;
		let remote_delayed_payment_base_key = Readable::read(reader)?;
		let remote_htlc_base_key = Readable::read(reader)?;
		let per_commitment_key = Readable::read(reader)?;
		let input_descriptor = Readable::read(reader)?;
		let amount = Readable::read(reader)?;
		let htlc = Readable::read(reader)?;
		let on_remote_tx_csv = Readable::read(reader)?;
		Ok(RevokedOutput {
			per_commitment_point,
			remote_delayed_payment_base_key,
			remote_htlc_base_key,
			per_commitment_key,
			input_descriptor,
			amount,
			htlc,
			on_remote_tx_csv
		})
	}
}

/// A struct to describe a remote htlc output, the templated witnessScript variables to claim it (hash,
/// timelock, pubkeys) and preimage to generate a solving witness. It is used by OnchainTxHandler
/// to generate a valid transaction claiming this output.
#[derive(Clone, PartialEq)]
pub(crate) struct RemoteHTLCOutput {
	per_commitment_point: PublicKey,
	remote_delayed_payment_base_key: PublicKey,
	remote_htlc_base_key: PublicKey,
	preimage: Option<PaymentPreimage>,
	htlc: HTLCOutputInCommitment
}

impl Writeable for RemoteHTLCOutput {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ::std::io::Error> {
		self.per_commitment_point.write(writer)?;
		self.remote_delayed_payment_base_key.write(writer)?;
		self.remote_htlc_base_key.write(writer)?;
		self.preimage.write(writer)?;
		self.htlc.write(writer)?;
		Ok(())
	}
}

impl Readable for RemoteHTLCOutput {
	fn read<R: ::std::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let per_commitment_point = Readable::read(reader)?;
		let remote_delayed_payment_base_key = Readable::read(reader)?;
		let remote_htlc_base_key = Readable::read(reader)?;
		let preimage = Readable::read(reader)?;
		let htlc = Readable::read(reader)?;
		Ok(RemoteHTLCOutput {
			per_commitment_point,
			remote_delayed_payment_base_key,
			remote_htlc_base_key,
			preimage,
			htlc
		})
	}
}

/// A struct to describe a local htlc output, amount and preimage to generate a signature and
/// solving witness. It is used by OnchainTxHandler to finalize a HTLC transaction claiming this
/// output.
#[derive(Clone, PartialEq)]
pub(crate) struct LocalHTLCOutput {
	preimage: Option<PaymentPreimage>,
	amount: u64,
}

impl Writeable for LocalHTLCOutput {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ::std::io::Error> {
		self.preimage.write(writer)?;
		writer.write_all(&byte_utils::be64_to_array(self.amount))?;
		Ok(())
	}
}

impl Readable for LocalHTLCOutput {
	fn read<R: ::std::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let preimage = Readable::read(reader)?;
		let amount = Readable::read(reader)?;
		Ok(LocalHTLCOutput {
			preimage,
			amount,
		})
	}
}

/// A struct to describe a local funding output with the static witnessScript to claim it. It is
/// used by OnchainTxHandler to finalize a local commitment transaction claiming this output.
#[derive(Clone, PartialEq)]
pub(crate) struct LocalFundingOutput {
	funding_redeemscript: Script,
}

impl Writeable for LocalFundingOutput {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ::std::io::Error> {
		self.funding_redeemscript.write(writer)?;
		Ok(())
	}
}

impl Readable for LocalFundingOutput {
	fn read<R: ::std::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		Ok(LocalFundingOutput {
			funding_redeemscript: Readable::read(reader)?,
		})
	}
}

/// An enum to describe a claim content which is generated by ChannelMonitor and
/// used by OnchainTxHandler to regenerate feerate-bump transactions to settle claims.
///
/// Template may be either malleable (a justice tx, a remote HTLC tx) or lockdown (a local htlc
/// tx, a local commitment tx, a pre-signed justice tx). Bumping can be a Replace-by-Fee, that way
/// the claim-settlement tx in itself has its feerate increased or Child-Pay-For-Parent, a child
/// of the claim tx has its feerate increased. For the latter case, access to the whole package
/// sizea and pre-committed fee is required to compute an efficient bump.
#[derive(Clone, PartialEq)]
pub(crate) enum PackageTemplate {
	MalleableJusticeTx {
		inputs: HashMap<BitcoinOutPoint, RevokedOutput>,
	},
	RemoteHTLCTx {
		inputs: HashMap<BitcoinOutPoint, RemoteHTLCOutput>,
	},
	LocalHTLCTx {
		input: (BitcoinOutPoint, LocalHTLCOutput),
	},
	LocalCommitmentTx {
		input: (BitcoinOutPoint, LocalFundingOutput),
	}
}

impl PackageTemplate {
	pub(crate) fn outpoints(&self) -> Vec<&BitcoinOutPoint> {
		match self {
			PackageTemplate::MalleableJusticeTx { ref inputs } => {
				inputs.keys().collect()
			},
			PackageTemplate::RemoteHTLCTx { ref inputs } => {
				inputs.keys().collect()
			},
			PackageTemplate::LocalHTLCTx { ref input } => {
				let mut outpoints = Vec::with_capacity(1);
				outpoints.push(&input.0);
				return outpoints;
			},
			PackageTemplate::LocalCommitmentTx { ref input } => {
				let mut outpoints = Vec::with_capacity(1);
				outpoints.push(&input.0);
				return outpoints;
			},
		}
	}
	pub(crate) fn package_split(&mut self, outp: &BitcoinOutPoint) -> Option<PackageTemplate> {
		match self {
			PackageTemplate::MalleableJusticeTx { ref mut inputs } => {
				if let Some(removed) = inputs.remove(outp) {
					let mut input_splitted = HashMap::with_capacity(1);
					input_splitted.insert(*outp, removed);
					return Some(PackageTemplate::MalleableJusticeTx {
						inputs: input_splitted,
					});
				}
				None
			},
			PackageTemplate::RemoteHTLCTx { ref mut inputs } => {
				if let Some(removed) = inputs.remove(outp) {
					let mut input_splitted = HashMap::with_capacity(1);
					input_splitted.insert(*outp, removed);
					return Some(PackageTemplate::RemoteHTLCTx {
						inputs: input_splitted,
					});
				}
				None
			},
			_ => {
				// Note, we may try to split on remote transaction for
				// which we don't have a competing one (HTLC-Success before
				// timelock expiration). This explain we don't panic!.
				// We should refactor OnchainTxHandler::block_connected to
				// only test equality on competing claims.
				return None;
			}
		}
	}
	pub(crate) fn package_merge(&mut self, mut template: PackageTemplate) {
		match self {
			PackageTemplate::MalleableJusticeTx { ref mut inputs } => {
				let base_inputs = inputs;
				match template {
					PackageTemplate::MalleableJusticeTx { ref mut inputs } => {
						for (k, v) in inputs.drain() {
							base_inputs.insert(k, v);
						}
					},
					_ => panic!("Merging templates of different types")
				}
			},
			PackageTemplate::RemoteHTLCTx { ref mut inputs } => {
				let base_inputs = inputs;
				match template {
					PackageTemplate::RemoteHTLCTx { ref mut inputs } => {
						for (k, v) in inputs.drain() {
							base_inputs.insert(k, v);
						}
					},
					_ => panic!("Merging templates of different types")
				}
			},
			_ => panic!("Merging template on non-malleable packages")
		}
	}
	pub(crate) fn package_amounts(&self) -> u64 {
		let amounts = match self {
			PackageTemplate::MalleableJusticeTx { ref inputs } => {
				let mut amounts = 0;
				for outp in inputs.values() {
					amounts += outp.amount;
				}
				amounts
			},
			PackageTemplate::RemoteHTLCTx { ref inputs } => {
				let mut amounts = 0;
				for outp in inputs.values() {
					amounts += outp.htlc.amount_msat / 1000;
				}
				amounts
			},
			_ => 0,
		};
		amounts
	}
	pub(crate) fn package_weight(&self, destination_script: &Script) -> usize {
		let mut input = Vec::new();
		let witnesses_weight = match self {
			PackageTemplate::MalleableJusticeTx { ref inputs } => {
				let mut weight = 0;
				for (outpoint, outp) in inputs.iter() {
					input.push(TxIn {
						previous_output: *outpoint,
						script_sig: Script::new(),
						sequence: 0xfffffffd,
						witness: Vec::new(),
					});
					weight += get_witnesses_weight(&[outp.input_descriptor]);
				}
				weight
			},
			PackageTemplate::RemoteHTLCTx { ref inputs } => {
				let mut weight = 0;
				for (outpoint, outp) in inputs.iter() {
					input.push(TxIn {
						previous_output: *outpoint,
						script_sig: Script::new(),
						sequence: 0xfffffffd,
						witness: Vec::new(),
					});

					weight += get_witnesses_weight(if outp.preimage.is_some() { &[InputDescriptors::OfferedHTLC] } else { &[InputDescriptors::ReceivedHTLC] });
				}
				weight
			},
			_ => { return 0 }
		};
		let bumped_tx = Transaction {
			version: 2,
			lock_time: 0,
			input,
			output: vec![TxOut {
				script_pubkey: destination_script.clone(),
				value: 0
			}],
		};
		bumped_tx.get_weight() + witnesses_weight
	}
	pub(crate) fn package_finalize<L: Deref, ChanSigner: ChannelKeys>(&self, onchain_handler: &mut OnchainTxHandler<ChanSigner>, value: u64, destination_script: Script, logger: &L) -> Option<Transaction>
		where L::Target: Logger,
	{
		let mut bumped_tx = Transaction {
			version: 2,
			lock_time: 0,
			input: vec![],
			output: vec![TxOut {
				script_pubkey: destination_script,
				value,
			}],
		};
		match self {
			PackageTemplate::MalleableJusticeTx { ref inputs } => {
				for outp in inputs.keys() {
					bumped_tx.input.push(TxIn {
						previous_output: *outp,
						script_sig: Script::new(),
						sequence: 0xfffffffd,
						witness: Vec::new(),
					});
				}
				for (i, (outp, revk)) in inputs.iter().enumerate() {
					log_trace!(logger, "Claiming outpoint {}:{}", outp.txid, outp.vout);
					if let Ok(chan_keys) = TxCreationKeys::new(&onchain_handler.secp_ctx, &revk.per_commitment_point, &revk.remote_delayed_payment_base_key, &revk.remote_htlc_base_key, &onchain_handler.key_storage.pubkeys().revocation_basepoint, &onchain_handler.key_storage.pubkeys().htlc_basepoint) {
						let witness_script = if let Some(ref htlc) = revk.htlc {
							chan_utils::get_htlc_redeemscript_with_explicit_keys(&htlc, &chan_keys.local_htlc_key, &chan_keys.remote_htlc_key, &chan_keys.revocation_key)
						} else {
							chan_utils::get_revokeable_redeemscript(&chan_keys.revocation_key, revk.on_remote_tx_csv, &chan_keys.local_delayed_payment_key)
						};

						if let Ok(sig) = onchain_handler.key_storage.sign_justice_transaction(&bumped_tx, i, revk.amount, &revk.per_commitment_key, &revk.htlc, revk.on_remote_tx_csv, &onchain_handler.secp_ctx) {
							bumped_tx.input[i].witness.push(sig.serialize_der().to_vec());
							bumped_tx.input[i].witness[0].push(SigHashType::All as u8);
							if revk.htlc.is_some() {
								bumped_tx.input[i].witness.push(chan_keys.revocation_key.clone().serialize().to_vec());
							} else {
								bumped_tx.input[i].witness.push(vec!(1));
							}
							bumped_tx.input[i].witness.push(witness_script.clone().into_bytes());
						} else { return None; }
						//TODO: panic ?
					}
				}
				log_trace!(logger, "Going to broadcast Penalty Transaction {}...", bumped_tx.txid());
				return Some(bumped_tx);
			},
			PackageTemplate::RemoteHTLCTx { ref inputs } => {
				for outp in inputs.keys() {
					bumped_tx.input.push(TxIn {
						previous_output: *outp,
						script_sig: Script::new(),
						sequence: 0xfffffffd,
						witness: Vec::new(),
					});
				}
				for (i, (outp, rem)) in inputs.iter().enumerate() {
					log_trace!(logger, "Claiming outpoint {}:{}", outp.txid, outp.vout);
					if let Ok(chan_keys) = TxCreationKeys::new(&onchain_handler.secp_ctx, &rem.per_commitment_point, &rem.remote_delayed_payment_base_key, &rem.remote_htlc_base_key, &onchain_handler.key_storage.pubkeys().revocation_basepoint, &onchain_handler.key_storage.pubkeys().htlc_basepoint) {
						let witness_script = chan_utils::get_htlc_redeemscript_with_explicit_keys(&rem.htlc, &chan_keys.local_htlc_key, &chan_keys.remote_htlc_key, &chan_keys.revocation_key);

						if !rem.preimage.is_some() { bumped_tx.lock_time = rem.htlc.cltv_expiry }; // Right now we don't aggregate time-locked transaction, if we do we should set lock_time before to avoid breaking hash computation
						if let Ok(sig) = onchain_handler.key_storage.sign_remote_htlc_transaction(&bumped_tx, i, &rem.htlc.amount_msat / 1000, &rem.per_commitment_point, &rem.htlc, &onchain_handler.secp_ctx) {
							bumped_tx.input[i].witness.push(sig.serialize_der().to_vec());
							bumped_tx.input[i].witness[0].push(SigHashType::All as u8);
							if let Some(preimage) = rem.preimage {
								bumped_tx.input[i].witness.push(preimage.0.to_vec());
							} else {
								// Due to BIP146 (MINIMALIF) this must be a zero-length element to relay.
								bumped_tx.input[i].witness.push(vec![]);
							}
							bumped_tx.input[i].witness.push(witness_script.clone().into_bytes());
						}
					}
				}
				log_trace!(logger, "Going to broadcast Claim Transaction {} claiming remote htlc output...", bumped_tx.txid());
				return Some(bumped_tx);
			},
			PackageTemplate::LocalHTLCTx { ref input } => {
				let htlc_tx = onchain_handler.get_fully_signed_htlc_tx(&input.0, &input.1.preimage);
				if let Some(htlc_tx) = htlc_tx {
					// Timer set to $NEVER given we can't bump tx without anchor outputs
					log_trace!(logger, "Going to broadcast Local HTLC-{} claiming HTLC output {} from {}...", if input.1.preimage.is_some() { "Success" } else { "Timeout" }, input.0.vout, input.0.txid);
					return Some(htlc_tx);
				}
				return None;
			},
			PackageTemplate::LocalCommitmentTx { ref input } => {
				let signed_tx = onchain_handler.get_fully_signed_local_tx(&input.1.funding_redeemscript).unwrap();
				// Timer set to $NEVER given we can't bump tx without anchor outputs
				log_trace!(logger, "Going to broadcast Local Transaction {} claiming funding output {} from {}...", signed_tx.txid(), input.0.vout, input.0.txid);
				return Some(signed_tx);
			}
		}
	}
	pub(crate) fn build_malleable_justice_tx(per_commitment_point: PublicKey, per_commitment_key: SecretKey, remote_delayed_payment_base_key: PublicKey, remote_htlc_base_key: PublicKey, input_descriptor: InputDescriptors, txid: Txid, vout: u32, amount: u64, htlc: Option<HTLCOutputInCommitment>, on_remote_tx_csv: u16) -> Self {
		let revk_outp = RevokedOutput {
			per_commitment_point,
			remote_delayed_payment_base_key,
			remote_htlc_base_key,
			per_commitment_key,
			input_descriptor,
			amount,
			htlc,
			on_remote_tx_csv,
		};
		let mut inputs = HashMap::with_capacity(1);
		inputs.insert(BitcoinOutPoint { txid, vout }, revk_outp);
		PackageTemplate::MalleableJusticeTx {
			inputs,
		}
	}
	pub(crate) fn build_remote_htlc_tx(per_commitment_point: PublicKey, remote_delayed_payment_base_key: PublicKey, remote_htlc_base_key: PublicKey, preimage: Option<PaymentPreimage>, htlc: HTLCOutputInCommitment, txid: Txid, vout: u32) -> Self {
		let remote_outp = RemoteHTLCOutput {
			per_commitment_point,
			remote_delayed_payment_base_key,
			remote_htlc_base_key,
			preimage,
			htlc
		};
		let mut inputs = HashMap::with_capacity(1);
		inputs.insert(BitcoinOutPoint { txid, vout }, remote_outp);
		PackageTemplate::RemoteHTLCTx  {
			inputs,
		}
	}
	pub(crate) fn build_local_htlc_tx(preimage: Option<PaymentPreimage>, amount: u64, txid: Txid, vout: u32) -> Self {
		let htlc_outp = LocalHTLCOutput {
			preimage,
			amount,
		};
		PackageTemplate::LocalHTLCTx {
			input: (BitcoinOutPoint { txid, vout }, htlc_outp)
		}
	}
	pub(crate) fn build_local_commitment_tx(funding_redeemscript: Script, txid: Txid, vout: u32) -> Self {
		let funding_outp = LocalFundingOutput {
			funding_redeemscript,
		};
		PackageTemplate::LocalCommitmentTx {
			input: (BitcoinOutPoint { txid, vout }, funding_outp)
		}
	}
}

impl Default for PackageTemplate {
	fn default() -> Self {
		PackageTemplate::MalleableJusticeTx {
			inputs: HashMap::new(),
		}
	}
}

impl Writeable for PackageTemplate {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ::std::io::Error> {
		match self {
			&PackageTemplate::MalleableJusticeTx { ref inputs } => {
				writer.write_all(&[0; 1])?;
				writer.write_all(&byte_utils::be64_to_array(inputs.len() as u64))?;
				for (ref outpoint, ref rev_outp) in inputs.iter() {
					outpoint.write(writer)?;
					rev_outp.write(writer)?;
				}
			},
			&PackageTemplate::RemoteHTLCTx { ref inputs } => {
				writer.write_all(&[1; 1])?;
				writer.write_all(&byte_utils::be64_to_array(inputs.len() as u64))?;
				for (ref outpoint, ref remote_outp) in inputs.iter() {
					outpoint.write(writer)?;
					remote_outp.write(writer)?;
				}
			},
			&PackageTemplate::LocalHTLCTx { ref input } => {
				writer.write_all(&[2; 1])?;
				input.0.write(writer)?;
				input.1.write(writer)?;
			},
			&PackageTemplate::LocalCommitmentTx { ref input } => {
				writer.write_all(&[3; 1])?;
				input.0.write(writer)?;
				input.1.write(writer)?;
			}
		}
		Ok(())
	}
}

impl Readable for PackageTemplate {
	fn read<R: ::std::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let package = match <u8 as Readable>::read(reader)? {
			0 => {
				let inputs_count = <u64 as Readable>::read(reader)?;
				let mut inputs = HashMap::with_capacity(cmp::min(inputs_count as usize, MAX_ALLOC_SIZE / 128));
				for _ in 0..inputs_count {
					let outpoint = Readable::read(reader)?;
					let rev_outp = Readable::read(reader)?;
					inputs.insert(outpoint, rev_outp);
				}
				PackageTemplate::MalleableJusticeTx {
					inputs,
				}
			},
			1 => {
				let inputs_count = <u64 as Readable>::read(reader)?;
				let mut inputs = HashMap::with_capacity(cmp::min(inputs_count as usize, MAX_ALLOC_SIZE / 128));
				for _ in 0..inputs_count {
					let outpoint = Readable::read(reader)?;
					let remote_outp = Readable::read(reader)?;
					inputs.insert(outpoint, remote_outp);
				}
				PackageTemplate::RemoteHTLCTx {
					inputs,
				}
			},
			2 => {
				let outpoint = Readable::read(reader)?;
				let htlc_outp = Readable::read(reader)?;
				PackageTemplate::LocalHTLCTx {
					input: (outpoint, htlc_outp)
				}
			},
			3 => {
				let outpoint = Readable::read(reader)?;
				let funding_outp = Readable::read(reader)?;
				PackageTemplate::LocalCommitmentTx {
					input: (outpoint, funding_outp)
				}
			},
			_ => return Err(DecodeError::InvalidValue),
		};
		Ok(package)
	}
}

/// BumpStrategy is a basic enum to encode a fee-committing strategy. We
/// may extend it in the future with other stategies like BYOF-input.
#[derive(PartialEq, Clone)]
pub(crate) enum BumpStrategy {
	RBF,
	CPFP
}

impl Writeable for BumpStrategy {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ::std::io::Error> {
		match self {
			BumpStrategy::RBF => {
				writer.write_all(&[0; 1])?;
			},
			BumpStrategy::CPFP => {
				writer.write_all(&[1; 1])?;
			}
		}
		Ok(())
	}
}

impl Readable for BumpStrategy {
	fn read<R: ::std::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let bump_strategy = match <u8 as Readable>::read(reader)? {
			0 => {
				BumpStrategy::RBF
			},
			1 => {
				BumpStrategy::CPFP
			},
			_ => return Err(DecodeError::InvalidValue),
		};
		Ok(bump_strategy)
	}
}

/// A structure to describe a claim content and its metadatas which is generated
/// by ChannelMonitor and used by OnchainTxHandler to generate feerate-competive
/// transactions.
///
/// Metadatas are related to multiple fields playing a role in packet lifetime.
/// Once issued, it may be aggregated with other requests if it's judged safe
/// and feerate opportunistic.
/// Current LN fees model, pre-committed fees with update_fee adjustement, means
/// that counter-signed transactions must be CPFP to be dynamically confirmed as a
/// bumping strategy. If transactions aren't lockdown (i.e justice transactions) we
/// may RBF them.
/// Feerate previous will serve as a feerate floor between different bumping attempts.
/// Height timer clocks these different bumping attempts.
/// Absolute timelock defines the block barrier at which claiming isn't exclusive
/// to us anymore and thus we MUST have get it solved before.
/// Height original serves as a packet timestamps to prune out claim in case of reorg.
/// Content embeds transactions elements to generate transaction. See PackageTemplate.
#[derive(PartialEq, Clone)]
pub struct OnchainRequest {
	// Timeout tx must have nLocktime set which means aggregating multiple
	// ones must take the higher nLocktime among them to satisfy all of them.
	// Sadly it has few pitfalls, a) it takes longuer to get fund back b) CLTV_DELTA
	// of a sooner-HTLC could be swallowed by the highest nLocktime of the HTLC set.
	// Do simplify we mark them as non-aggregable.
	pub(crate) aggregation: bool,
	// Content may lockdown with counter-signature of our counterparty
	// or fully-malleable by our own. Depending on this bumping strategy
	// must be adapted.
	pub(crate) bump_strategy: BumpStrategy,
	// Based feerate of previous broadcast. If resources available (either
	// output value or utxo bumping).
	pub(crate) feerate_previous: u64,
	// At every block tick, used to check if pending claiming tx is taking too
	// much time for confirmation and we need to bump it.
	pub(crate) height_timer: Option<u32>,
	// Block height before which claiming is exclusive to one party,
	// after reaching it, claiming may be contentious.
	pub(crate) absolute_timelock: u32,
	// Tracked in case of reorg to wipe out now-superflous request.
	pub(crate) height_original: u32,
	// Content of request.
	pub(crate) content: PackageTemplate,
}

impl OnchainRequest {
	pub(crate) fn request_merge(&mut self, req: OnchainRequest) {
		// We init default onchain request with first merge content
		if self.absolute_timelock == ::std::u32::MAX {
			println!("Init merging {}", req.height_original);
			self.height_original = req.height_original;
			self.content = req.content;
			self.absolute_timelock = req.absolute_timelock;
			return;
		}
		assert_eq!(self.height_original, req.height_original);
		if self.absolute_timelock > req.absolute_timelock {
			self.absolute_timelock = req.absolute_timelock;
		}
		self.content.package_merge(req.content);
	}
}

impl Default for OnchainRequest {
	fn default() -> Self {
		OnchainRequest {
			aggregation: true,
			bump_strategy: BumpStrategy::RBF,
			feerate_previous: 0,
			height_timer: None,
			absolute_timelock: ::std::u32::MAX,
			height_original: 0,
			content: PackageTemplate::default()
		}
	}
}

impl Writeable for OnchainRequest {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ::std::io::Error> {
		self.aggregation.write(writer)?;
		self.bump_strategy.write(writer)?;
		self.feerate_previous.write(writer)?;
		self.height_timer.write(writer)?;
		self.absolute_timelock.write(writer)?;
		self.height_original.write(writer)?;
		self.content.write(writer)?;

		Ok(())
	}
}

impl Readable for OnchainRequest {
	fn read<R: ::std::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let aggregation = Readable::read(reader)?;
		let bump_strategy = Readable::read(reader)?;
		let feerate_previous = Readable::read(reader)?;
		let height_timer = Readable::read(reader)?;
		let absolute_timelock = Readable::read(reader)?;
		let height_original = Readable::read(reader)?;
		let content = Readable::read(reader)?;

		Ok(OnchainRequest {
			aggregation,
			bump_strategy,
			feerate_previous,
			height_timer,
			absolute_timelock,
			height_original,
			content
		})
	}
}

fn subtract_high_prio_fee<F: Deref, L: Deref>(input_amounts: u64, predicted_weight: usize, fee_estimator: &F, logger: &L) -> Option<(u64, u64)>
	where F::Target: FeeEstimator,
	      L::Target: Logger,
{
	let mut updated_feerate = fee_estimator.get_est_sat_per_1000_weight(ConfirmationTarget::HighPriority);
	let mut fee = updated_feerate * (predicted_weight as u64) / 1000;
	if input_amounts <= fee {
		updated_feerate = fee_estimator.get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
		fee = updated_feerate * (predicted_weight as u64) / 1000;
		if input_amounts <= fee {
			updated_feerate = fee_estimator.get_est_sat_per_1000_weight(ConfirmationTarget::Background);
			fee = updated_feerate * (predicted_weight as u64) / 1000;
			if input_amounts <= fee {
				log_error!(logger, "Failed to generate an on-chain punishment tx as even low priority fee ({} sat) was more than the entire claim balance ({} sat)",
					fee, input_amounts);
				None
			} else {
				log_warn!(logger, "Used low priority fee for on-chain punishment tx as high priority fee was more than the entire claim balance ({} sat)",
					input_amounts);
				Some((fee, updated_feerate))
			}
		} else {
			log_warn!(logger, "Used medium priority fee for on-chain punishment tx as high priority fee was more than the entire claim balance ({} sat)",
				input_amounts);
			Some((fee, updated_feerate))
		}
	} else {
		Some((fee, updated_feerate))
	}
}

fn feerate_bump<F: Deref, L: Deref>(predicted_weight: usize, input_amounts: u64, previous_feerate: u64, fee_estimator: &F, logger: &L) -> Option<(u64, u64)>
	where F::Target: FeeEstimator,
	      L::Target: Logger,
{
	// If old feerate inferior to actual one given back by Fee Estimator, use it to compute new fee...
	let new_fee = if previous_feerate < fee_estimator.get_est_sat_per_1000_weight(ConfirmationTarget::HighPriority) {
		if let Some((new_fee, _)) = subtract_high_prio_fee(input_amounts, predicted_weight, fee_estimator, logger) {
			new_fee
		} else {
			log_trace!(logger, "Can't new-estimation bump new claiming tx, amount {} is too small", input_amounts);
			return None;
		}
	// ...else just increase the previous feerate by 25% (because that's a nice number)
	} else {
		let fee = previous_feerate * (predicted_weight as u64) / 750;
		if input_amounts <= fee {
			log_trace!(logger, "Can't 25% bump new claiming tx, amount {} is too small", input_amounts);
			return None;
		}
		fee
	};

	let previous_fee = previous_feerate * (predicted_weight as u64) / 1000;
	let min_relay_fee = MIN_RELAY_FEE_SAT_PER_1000_WEIGHT * (predicted_weight as u64) / 1000;
	// BIP 125 Opt-in Full Replace-by-Fee Signaling
	// 	* 3. The replacement transaction pays an absolute fee of at least the sum paid by the original transactions.
	//	* 4. The replacement transaction must also pay for its own bandwidth at or above the rate set by the node's minimum relay fee setting.
	let new_fee = if new_fee < previous_fee + min_relay_fee {
		new_fee + previous_fee + min_relay_fee - new_fee
	} else {
		new_fee
	};
	Some((new_fee, new_fee * 1000 / (predicted_weight as u64)))
}

pub(crate) fn compute_output_value<F: Deref, L: Deref>(predicted_weight: usize, input_amounts: u64, previous_feerate: u64, fee_estimator: &F, logger: &L) -> Option<(u64, u64)>
	where F::Target: FeeEstimator,
	      L::Target: Logger,
{
	// If old feerate is 0, first iteration of this claim, use normal fee calculation
	if previous_feerate != 0 {
		if let Some((new_fee, feerate)) = feerate_bump(predicted_weight, input_amounts, previous_feerate, fee_estimator, logger) {
			// If new computed fee is superior at the whole claimable amount burn all in fees
			if new_fee > input_amounts {
				return Some((0, feerate));
			} else {
				return Some((input_amounts - new_fee, feerate));
			}
		}
	} else {
		if let Some((new_fee, feerate)) = subtract_high_prio_fee(input_amounts, predicted_weight, fee_estimator, logger) {
				return Some((input_amounts - new_fee, feerate));
		}
	}
	None
}
