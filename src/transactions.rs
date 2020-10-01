//! Revault transactions
//!
//! Typesafe routines to create bare revault transactions.

use crate::{error::Error, txins::*, txouts::*};

use bitcoin::{
    consensus::encode::{Encodable, Error as EncodeError},
    hashes::{hash160::Hash as Hash160, Hash},
    util::{
        bip143::SigHashCache,
        psbt::{
            Global as PsbtGlobal, Input as PsbtIn, Output as PsbtOut,
            PartiallySignedTransaction as Psbt,
        },
    },
    OutPoint, PublicKey, Script, SigHash, SigHashType, Transaction, TxOut,
};
use miniscript::{BitcoinSig, MiniscriptKey, Satisfier, ToPublicKey};

use std::collections::{BTreeMap, HashMap};
use std::fmt;

/// TxIn's sequence to set for the tx to be bip125-replaceable
pub const RBF_SEQUENCE: u32 = u32::MAX - 2;

/// A Revault transaction. Apart from the VaultTransaction, all variants must be instanciated
/// using the new_*() methods.
pub trait RevaultTransaction: fmt::Debug + Clone + PartialEq {
    /// Get the inner transaction
    fn inner_tx(&self) -> &Psbt;

    /// Get the inner transaction
    fn inner_tx_mut(&mut self) -> &mut Psbt;

    /// Add a signature in order to eventually satisfy this input.
    /// The BIP174 Signer.
    fn add_signature(
        &mut self,
        input_index: usize,
        pubkey: bitcoin::PublicKey,
        rawsig: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, Error> {
        if let Some(ref mut psbtin) = self.inner_tx_mut().inputs.get_mut(input_index) {
            Ok(psbtin.partial_sigs.insert(pubkey, rawsig))
        } else {
            Err(Error::InputSatisfaction(format!(
                "Input out of bonds of PSBT inputs: {:?}",
                self.inner_tx().inputs
            )))
        }
    }

    /// Check and satisfy the scripts, create the witnesses.
    /// The BIP174 Input Finalizer
    fn finalize(&mut self) -> Result<(), Error> {
        let psbt = self.inner_tx_mut();
        let (psbt_inputs, tx_inputs) = (&mut psbt.inputs, &psbt.global.unsigned_tx.input);

        if psbt_inputs.len() != tx_inputs.len() {
            return Err(Error::TransactionFinalisation(format!(
                "Number of inputs mismatch. The PSBT has {}, the unsigned transaction has {}.",
                psbt_inputs.len(),
                tx_inputs.len()
            )));
        }

        // FIXME: Check sighash type and signatures

        for (psbtin, txin) in psbt_inputs.iter_mut().zip(tx_inputs.iter()) {
            let prev_txo = match psbtin.witness_utxo.clone() {
                Some(utxo) => utxo,
                None => {
                    return Err(Error::TransactionFinalisation(format!(
                        "Missing witness utxo for psbt input '{:?}'",
                        psbtin
                    )))
                }
            };

            // This stores the hash=>key mapping, so we need it early to construct the P2WPKH
            // descriptor
            let input_satisfier =
                RevaultInputSatisfier::new(&mut psbtin.partial_sigs, txin.sequence);

            // We might need to satisfy a P2WPKH (eg the feebump input). That's the "simple" case,
            // we can do it by hand (at least until upstream is done implementing PSBTs +
            // Miniscript desriptors).
            // We marshal the PKH out of the ScriptPubKey and directly gather the sig from our
            // satisfier.
            if prev_txo.script_pubkey.is_v0_p2wpkh() {
                // A P2WPKH is 0 PUSH<hash>, so we want the second instruction.
                let hash = match &prev_txo.script_pubkey.instructions_minimal().nth(1) {
                    Some(Ok(bitcoin::blockdata::script::Instruction::PushBytes(bytes))) => {
                        Hash160::from_slice(bytes).map_err(|e| {
                            Error::TransactionFinalisation(format!(
                                "Could not parse public key hash in P2WPKH script pubkey: {}",
                                e
                            ))
                        })
                    }
                    _ => {
                        return Err(Error::TransactionFinalisation(format!(
                            "Invalid witness utxo given by psbt input '{:?}': invalid P2WPKH",
                            psbtin
                        )))
                    }
                }?;

                let pk: bitcoin::PublicKey =
                    input_satisfier.lookup_pkh_pk(&hash).ok_or_else(|| {
                        Error::TransactionFinalisation(format!(
                            "Could not find pubkey associated with hash '{:x?}'",
                            hash
                        ))
                    })?;
                let sig = input_satisfier.lookup_sig(&pk).ok_or_else(|| {
                    Error::TransactionFinalisation(format!("No signature for pubkey '{:x?}'", pk))
                })?;
                let mut sig_der = sig.0.serialize_der().to_vec();
                // FIXME: Check the sighash here
                sig_der.push(sig.1.as_u32() as u8);

                psbtin.final_script_witness = Some(vec![sig_der, pk.to_public_key().to_bytes()]);

            // In the standard case, we (re)construct a Miniscript out of the witness script in
            // order to have a comprehensive and adequate satisfaction, then we push the actual
            // witness script.
            } else if prev_txo.script_pubkey.is_v0_p2wsh() {
                let prev_script = match psbtin.witness_script {
                    Some(ref script) => {
                        match miniscript::Miniscript::<_, miniscript::Segwitv0>::parse(script) {
                            Ok(miniscript) => miniscript,
                            Err(e) => {
                                return Err(Error::TransactionFinalisation(format!(
                                    "Could not parse witness script for psbt input '{:?}' : {:?}",
                                    psbtin, e
                                )))
                            }
                        }
                    }
                    None => {
                        return Err(Error::TransactionFinalisation(format!(
                            "Missing witness script for psbt input '{:?}'",
                            psbtin
                        )))
                    }
                };

                match prev_script.satisfy(&input_satisfier) {
                    Some(mut witness) => {
                        witness.push(prev_script.encode().into_bytes());
                        psbtin.final_script_witness = Some(witness);
                    }
                    None => {
                        return Err(Error::TransactionFinalisation(format!(
                        "Input satisfaction error for PSBT input '{:?}' and witness script '{:?}'",
                        psbtin, prev_script
                    )))
                    }
                }
            } else {
                return Err(Error::TransactionFinalisation(format!(
                    "Invalid previous txout type for psbt input '{:?}'.",
                    psbtin,
                )));
            }
        }

        Ok(())
    }

    /// Get the specified output of this transaction as an OutPoint to be referenced
    /// in a following transaction.
    fn into_outpoint(&self, vout: u32) -> OutPoint {
        OutPoint {
            txid: self.inner_tx().global.unsigned_tx.txid(),
            vout,
        }
    }

    /// Get the network-serialized (inner) transaction. You likely want to call [finalize] before
    /// serializing the transaction.
    /// The BIP174 Transaction Extractor (without any check, which are done in [finalize]).
    fn as_bitcoin_serialized(&self) -> Result<Vec<u8>, EncodeError> {
        let mut buff = Vec::<u8>::new();
        self.inner_tx()
            .clone()
            .extract_tx()
            .consensus_encode(&mut buff)?;
        Ok(buff)
    }

    /// Get the BIP174-serialized (inner) transaction.
    fn as_psbt_serialized(&self) -> Result<Vec<u8>, EncodeError> {
        let mut buff = Vec::<u8>::new();
        self.inner_tx().consensus_encode(&mut buff)?;
        Ok(buff)
    }

    /// Get the hexadecimal representation of the transaction as used by the bitcoind API.
    ///
    /// # Errors
    /// - If we could not encode the transaction (should not happen).
    fn hex(&self) -> Result<String, EncodeError> {
        let buff = self.as_bitcoin_serialized()?;
        let mut as_hex = String::new();

        for byte in buff.into_iter() {
            as_hex.push_str(&format!("{:02x}", byte));
        }

        Ok(as_hex)
    }
}

// Boilerplate for newtype declaration and small trait helpers implementation.
macro_rules! impl_revault_transaction {
    ( $transaction_name:ident, $doc_comment:meta ) => {
        #[$doc_comment]
        #[derive(Debug, Clone, PartialEq)]
        pub struct $transaction_name(Psbt);

        impl RevaultTransaction for $transaction_name {
            fn inner_tx(&self) -> &Psbt {
                &self.0
            }

            fn inner_tx_mut(&mut self) -> &mut Psbt {
                &mut self.0
            }
        }
    };
}

// Boilerplate for creating an actual (inner) transaction with a known number of prevouts / txouts.
macro_rules! create_tx {
    ( [$($revault_txin:expr),* $(,)?], [$($txout:expr),* $(,)?], $lock_time:expr $(,)?) => {
        Psbt {
            global: PsbtGlobal {
                unsigned_tx: Transaction {
                    version: 2,
                    lock_time: $lock_time,
                    input: vec![$(
                        $revault_txin.as_unsigned_txin(),
                    )*],
                    output: vec![$(
                        $txout.clone().get_txout(),
                    )*],
                },
                unknown: BTreeMap::new(),
            },
            inputs: vec![$(
                PsbtIn {
                    witness_script: $revault_txin.clone().into_txout().into_witness_script(),
                    sighash_type: None, // FIXME
                    witness_utxo: Some($revault_txin.into_txout().get_txout()),
                    ..PsbtIn::default()
                },
            )*],
            outputs: vec![$(
                PsbtOut {
                    witness_script: $txout.into_witness_script(),
                    ..PsbtOut::default()
                },
            )*],
        }
    }
}

impl_revault_transaction!(
    UnvaultTransaction,
    doc = "The unvaulting transaction, spending a vault and being eventually spent by a spend transaction (if not revaulted)."
);
impl UnvaultTransaction {
    /// An unvault transaction always spends one vault output and contains one CPFP output in
    /// addition to the unvault one.
    /// PSBT Creator and Updater.
    pub fn new(
        vault_input: VaultTxIn,
        unvault_txout: UnvaultTxOut,
        cpfp_txout: CpfpTxOut,
        lock_time: u32,
    ) -> UnvaultTransaction {
        UnvaultTransaction(create_tx!(
            [vault_input],
            [unvault_txout, cpfp_txout],
            lock_time,
        ))
    }
}

impl_revault_transaction!(
    CancelTransaction,
    doc = "The transaction \"revaulting\" a spend attempt, i.e. spending the unvaulting transaction back to a vault txo."
);
impl CancelTransaction {
    /// A cancel transaction always pays to a vault output and spends the unvault output, and
    /// may have a fee-bumping input.
    /// PSBT Creator and Updater.
    pub fn new(
        unvault_input: UnvaultTxIn,
        feebump_input: Option<FeeBumpTxIn>,
        vault_txout: VaultTxOut,
        lock_time: u32,
    ) -> CancelTransaction {
        CancelTransaction(if let Some(feebump_input) = feebump_input {
            create_tx!([unvault_input, feebump_input], [vault_txout], lock_time,)
        } else {
            create_tx!([unvault_input], [vault_txout], lock_time,)
        })
    }
}

impl_revault_transaction!(
    EmergencyTransaction,
    doc = "The transaction spending a vault output to The Emergency Script."
);
impl EmergencyTransaction {
    /// The first emergency transaction always spends a vault output and pays to the Emergency
    /// Script. It may also spend an additional output for fee-bumping.
    /// PSBT Creator and Updater.
    pub fn new(
        vault_input: VaultTxIn,
        feebump_input: Option<FeeBumpTxIn>,
        emer_txout: EmergencyTxOut,
        lock_time: u32,
    ) -> EmergencyTransaction {
        EmergencyTransaction(if let Some(feebump_input) = feebump_input {
            create_tx!([vault_input, feebump_input], [emer_txout], lock_time,)
        } else {
            create_tx!([vault_input], [emer_txout], lock_time,)
        })
    }
}

impl_revault_transaction!(
    UnvaultEmergencyTransaction,
    doc = "The transaction spending an unvault output to The Emergency Script."
);
impl UnvaultEmergencyTransaction {
    /// The second emergency transaction always spends an unvault output and pays to the Emergency
    /// Script. It may also spend an additional output for fee-bumping.
    /// PSBT Creator and Updater.
    pub fn new(
        unvault_input: UnvaultTxIn,
        feebump_input: Option<FeeBumpTxIn>,
        emer_txout: EmergencyTxOut,
        lock_time: u32,
    ) -> UnvaultEmergencyTransaction {
        UnvaultEmergencyTransaction(if let Some(feebump_input) = feebump_input {
            create_tx!([unvault_input, feebump_input], [emer_txout], lock_time,)
        } else {
            create_tx!([unvault_input], [emer_txout], lock_time,)
        })
    }
}

impl_revault_transaction!(
    SpendTransaction,
    doc = "The transaction spending the unvaulting transaction, paying to one or multiple \
    externally-controlled addresses, and possibly to a new vault txo for the change."
);
impl SpendTransaction {
    /// A spend transaction can batch multiple unvault txouts, and may have any number of
    /// txouts (including, but not restricted to, change).
    /// PSBT Creator and Updater.
    pub fn new(
        unvault_inputs: Vec<UnvaultTxIn>,
        spend_txouts: Vec<SpendTxOut>,
        lock_time: u32,
    ) -> SpendTransaction {
        SpendTransaction(Psbt {
            global: PsbtGlobal {
                unsigned_tx: Transaction {
                    version: 2,
                    lock_time,
                    input: unvault_inputs
                        .iter()
                        .map(|input| input.as_unsigned_txin())
                        .collect(),
                    output: spend_txouts
                        .iter()
                        .map(|spend_txout| match spend_txout {
                            SpendTxOut::Destination(ref txo) => txo.clone().get_txout(),
                            SpendTxOut::Change(ref txo) => txo.clone().get_txout(),
                        })
                        .collect(),
                },
                unknown: BTreeMap::new(),
            },
            inputs: unvault_inputs
                .into_iter()
                .map(|input| {
                    let prev_txout = input.into_txout();
                    PsbtIn {
                        witness_script: prev_txout.witness_script().clone(),
                        sighash_type: None, // FIXME
                        witness_utxo: Some(prev_txout.get_txout()),
                        ..PsbtIn::default()
                    }
                })
                .collect(),
            outputs: spend_txouts
                .into_iter()
                .map(|spend_txout| PsbtOut {
                    witness_script: match spend_txout {
                        SpendTxOut::Destination(txo) => txo.into_witness_script(),
                        SpendTxOut::Change(txo) => txo.into_witness_script(),
                    },
                    ..PsbtOut::default()
                })
                .collect(),
        })
    }
}

impl_revault_transaction!(
    VaultTransaction,
    doc = "The funding transaction, we don't create nor sign it."
);
impl VaultTransaction {
    /// We don't create nor are able to sign, it's just a type wrapper so explicitly no
    /// restriction on the types here
    pub fn new(psbt: Psbt) -> VaultTransaction {
        VaultTransaction(psbt)
    }
}

impl_revault_transaction!(
    FeeBumpTransaction,
    doc = "The fee-bumping transaction, we don't create nor sign it."
);
impl FeeBumpTransaction {
    /// We don't create nor are able to sign, it's just a type wrapper so explicitly no
    /// restriction on the types here
    pub fn new(psbt: Psbt) -> FeeBumpTransaction {
        FeeBumpTransaction(psbt)
    }
}

// Non typesafe sighash boilerplate
fn sighash(
    psbt: &Psbt,
    input_index: usize,
    previous_txout: &TxOut,
    script_code: &Script,
    is_anyonecanpay: bool,
) -> SigHash {
    // FIXME: cache the cache for when the user has too much cash
    let mut cache = SigHashCache::new(&psbt.global.unsigned_tx);
    cache.signature_hash(
        input_index,
        &script_code,
        previous_txout.value,
        if is_anyonecanpay {
            SigHashType::AllPlusAnyoneCanPay
        } else {
            SigHashType::All
        },
    )
}

// We use this to configure which txouts types are valid to be used by a given transaction type.
// This allows to compile-time check that we request a sighash for what is more likely to be a
// valid Revault transaction.
macro_rules! impl_valid_prev_txouts {
    ( $valid_prev_txouts: ident, [$($txout:ident),*], $doc_comment:meta ) => {
        #[$doc_comment]
        pub trait $valid_prev_txouts: RevaultTxOut {}
        $(impl $valid_prev_txouts for $txout {})*
    };
}

impl UnvaultTransaction {
    /// Get a signature hash for an input, previous_txout's type is statically checked to be
    /// acceptable.
    pub fn signature_hash(
        &self,
        input_index: usize,
        previous_txout: &VaultTxOut,
        script_code: &Script,
    ) -> SigHash {
        sighash(
            &self.0,
            input_index,
            previous_txout.inner_txout(),
            script_code,
            false,
        )
    }
}

impl_valid_prev_txouts!(
    CancelPrevTxout,
    [UnvaultTxOut, FeeBumpTxOut],
    doc = "CancelTransaction can only spend UnvaultTxOut and FeeBumpTxOut txouts"
);
impl CancelTransaction {
    /// Get a signature hash for an input, previous_txout's type is statically checked to be
    /// acceptable.
    pub fn signature_hash(
        &self,
        input_index: usize,
        previous_txout: &impl CancelPrevTxout,
        script_code: &Script,
        is_anyonecanpay: bool,
    ) -> SigHash {
        sighash(
            &self.0,
            input_index,
            previous_txout.inner_txout(),
            script_code,
            is_anyonecanpay,
        )
    }
}

impl_valid_prev_txouts!(
    EmergencyPrevTxout,
    [VaultTxOut, FeeBumpTxOut],
    doc = "EmergencyTransaction can only spend UnvaultTxOut and FeeBumpTxOut txouts"
);
impl EmergencyTransaction {
    /// Get a signature hash for an input, previous_txout's type is statically checked to be
    /// acceptable.
    pub fn signature_hash(
        &self,
        input_index: usize,
        previous_txout: &impl EmergencyPrevTxout,
        script_code: &Script,
        is_anyonecanpay: bool,
    ) -> SigHash {
        sighash(
            &self.0,
            input_index,
            previous_txout.inner_txout(),
            script_code,
            is_anyonecanpay,
        )
    }
}

impl_valid_prev_txouts!(
    UnvaultEmerPrevTxout,
    [UnvaultTxOut, FeeBumpTxOut],
    doc = "UnvaultEmergencyTransaction can only spend UnvaultTxOut and FeeBumpTxOut txouts."
);
impl UnvaultEmergencyTransaction {
    /// Get a signature hash for an input, previous_txout's type is statically checked to be
    /// acceptable.
    fn signature_hash(
        &self,
        input_index: usize,
        previous_txout: &impl UnvaultEmerPrevTxout,
        script_code: &Script,
        is_anyonecanpay: bool,
    ) -> SigHash {
        sighash(
            &self.0,
            input_index,
            previous_txout.inner_txout(),
            script_code,
            is_anyonecanpay,
        )
    }
}

impl SpendTransaction {
    /// Get a signature hash for an input, previous_txout's type is statically checked to be
    /// acceptable.
    pub fn signature_hash(
        &self,
        input_index: usize,
        previous_txout: &UnvaultTxOut,
        script_code: &Script,
    ) -> SigHash {
        sighash(
            &self.0,
            input_index,
            previous_txout.inner_txout(),
            script_code,
            false,
        )
    }
}

// A small wrapper to ease input satisfaction that won't be needed after:
// - https://github.com/rust-bitcoin/rust-bitcoin/pull/478
// - https://github.com/rust-bitcoin/rust-miniscript/pull/121
// - https://github.com/rust-bitcoin/rust-miniscript/pull/137
// - https://github.com/rust-bitcoin/rust-miniscript/pull/119
//
// But, for obvious reasons i did not want to rely again on hacked branches rebasing all of this,
// so the satisfaction of a PSBT input is (re-)implemented here.
struct RevaultInputSatisfier<'a> {
    pkhashmap: HashMap<Hash160, bitcoin::PublicKey>,
    // Raw sig as pushed on the witness stack, same as in the Psbt input struct
    sigmap: &'a mut BTreeMap<bitcoin::PublicKey, Vec<u8>>,
    sequence: u32,
    // FIXME: Add the sighash type from the PsbtIn here to be even more zealous!
}

impl<'a> RevaultInputSatisfier<'a> {
    fn new(
        sigmap: &'a mut BTreeMap<bitcoin::PublicKey, Vec<u8>>,
        sequence: u32,
    ) -> RevaultInputSatisfier {
        // This hack isn't going to last, see above.
        let mut pkhashmap = HashMap::<Hash160, bitcoin::PublicKey>::new();
        sigmap.keys().for_each(|pubkey| {
            pkhashmap.insert(pubkey.to_pubkeyhash(), *pubkey);
        });

        RevaultInputSatisfier {
            sequence,
            sigmap,
            pkhashmap,
        }
    }
}

impl Satisfier<bitcoin::PublicKey> for RevaultInputSatisfier<'_> {
    fn lookup_sig(&self, pk: &bitcoin::PublicKey) -> Option<BitcoinSig> {
        if let Some(rawsig) = self.sigmap.get(&pk.to_public_key()) {
            let (flag, sig) = match rawsig.split_last() {
                Some((f, s)) => (f, s),
                None => return None,
            };
            let flag = bitcoin::SigHashType::from_u32((*flag).into());
            let sig = match bitcoin::secp256k1::Signature::from_der(sig) {
                Ok(sig) => sig,
                Err(..) => return None,
            };

            Some((sig, flag))
        } else {
            None
        }
    }

    fn lookup_pkh_pk(&self, keyhash: &Hash160) -> Option<bitcoin::PublicKey> {
        self.pkhashmap.get(keyhash).copied()
    }

    // The policy compiler will often optimize the Script to use pkH, so we need this method to be
    // implemented *both* for satisfaction and disatisfaction !
    fn lookup_pkh_sig(&self, keyhash: &Hash160) -> Option<(PublicKey, BitcoinSig)> {
        self.lookup_pkh_pk(keyhash).and_then(|key| {
            if let Some(sig) = self.lookup_sig(&key) {
                Some((key, sig))
            } else {
                None
            }
        })
    }

    fn check_older(&self, csv: u32) -> bool {
        assert!((csv & (1 << 22) == 0));
        self.sequence >= csv
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CancelTransaction, EmergencyTransaction, FeeBumpTransaction, Psbt, RevaultTransaction,
        SpendTransaction, UnvaultEmergencyTransaction, UnvaultTransaction, VaultTransaction,
        RBF_SEQUENCE,
    };
    use crate::{scripts::*, txins::*, txouts::*};

    use std::str::FromStr;

    use bitcoin::{
        secp256k1::rand::{rngs::SmallRng, FromEntropy, RngCore},
        util::bip32,
        OutPoint, SigHash, SigHashType, Transaction, TxIn, TxOut,
    };
    use miniscript::{
        descriptor::{DescriptorPublicKey, DescriptorXPub},
        Descriptor, ToPublicKey,
    };

    fn get_random_privkey(rng: &mut SmallRng) -> bip32::ExtendedPrivKey {
        let mut rand_bytes = [0u8; 64];

        rng.fill_bytes(&mut rand_bytes);

        bip32::ExtendedPrivKey::new_master(
            bitcoin::network::constants::Network::Bitcoin,
            &rand_bytes,
        )
        .unwrap_or_else(|_| get_random_privkey(rng))
    }

    /// This generates the master private keys to derive directly from master, so it's
    /// [None]<xpub_goes_here>m/* descriptor pubkeys
    fn get_participants_sets(
        secp: &bitcoin::secp256k1::Secp256k1<bitcoin::secp256k1::All>,
    ) -> (
        (Vec<bip32::ExtendedPrivKey>, Vec<DescriptorPublicKey>),
        (Vec<bip32::ExtendedPrivKey>, Vec<DescriptorPublicKey>),
        (Vec<bip32::ExtendedPrivKey>, Vec<DescriptorPublicKey>),
    ) {
        let mut rng = SmallRng::from_entropy();

        let managers_priv = (0..3)
            .map(|_| get_random_privkey(&mut rng))
            .collect::<Vec<bip32::ExtendedPrivKey>>();
        let managers = managers_priv
            .iter()
            .map(|xpriv| {
                DescriptorPublicKey::XPub(DescriptorXPub {
                    origin: None,
                    xpub: bip32::ExtendedPubKey::from_private(&secp, &xpriv),
                    derivation_path: bip32::DerivationPath::from(vec![]),
                    is_wildcard: true,
                })
            })
            .collect::<Vec<DescriptorPublicKey>>();

        let non_managers_priv = (0..8)
            .map(|_| get_random_privkey(&mut rng))
            .collect::<Vec<bip32::ExtendedPrivKey>>();
        let non_managers = non_managers_priv
            .iter()
            .map(|xpriv| {
                DescriptorPublicKey::XPub(DescriptorXPub {
                    origin: None,
                    xpub: bip32::ExtendedPubKey::from_private(&secp, &xpriv),
                    derivation_path: bip32::DerivationPath::from(vec![]),
                    is_wildcard: true,
                })
            })
            .collect::<Vec<DescriptorPublicKey>>();

        let cosigners_priv = (0..8)
            .map(|_| get_random_privkey(&mut rng))
            .collect::<Vec<bip32::ExtendedPrivKey>>();
        let cosigners = cosigners_priv
            .iter()
            .map(|xpriv| {
                DescriptorPublicKey::XPub(DescriptorXPub {
                    origin: None,
                    xpub: bip32::ExtendedPubKey::from_private(&secp, &xpriv),
                    derivation_path: bip32::DerivationPath::from(vec![]),
                    is_wildcard: true,
                })
            })
            .collect::<Vec<DescriptorPublicKey>>();

        (
            (managers_priv, managers),
            (non_managers_priv, non_managers),
            (cosigners_priv, cosigners),
        )
    }

    // Routine for ""signing"" a transaction
    fn satisfy_transaction_input(
        secp: &bitcoin::secp256k1::Secp256k1<bitcoin::secp256k1::All>,
        tx: &mut impl RevaultTransaction,
        input_index: usize,
        tx_sighash: &SigHash,
        xprivs: &Vec<bip32::ExtendedPrivKey>,
        child_number: Option<bip32::ChildNumber>,
        is_anyonecanpay: bool,
    ) {
        // Can we agree that rustfmt does some nasty formatting now ??
        let derivation_path = bip32::DerivationPath::from(if let Some(cn) = child_number {
            vec![cn]
        } else {
            vec![]
        });
        xprivs.iter().for_each(|xpriv| {
            let mut sig = secp
                .sign(
                    &bitcoin::secp256k1::Message::from_slice(&tx_sighash).unwrap(),
                    &xpriv
                        .derive_priv(&secp, &derivation_path)
                        .unwrap()
                        .private_key
                        .key,
                )
                .serialize_der()
                .to_vec();
            sig.push(if is_anyonecanpay {
                SigHashType::AllPlusAnyoneCanPay.as_u32() as u8
            } else {
                SigHashType::All.as_u32() as u8
            });

            tx.add_signature(
                input_index,
                DescriptorPublicKey::XPub(DescriptorXPub {
                    origin: None,
                    xpub: bip32::ExtendedPubKey::from_private(&secp, xpriv),
                    derivation_path: derivation_path.clone(),
                    is_wildcard: true,
                })
                .to_public_key(),
                sig,
            )
            .unwrap();
        });
    }

    // FIXME: make it return an error and expose it to the world
    macro_rules! assert_libbitcoinconsensus_validity {
        ( $tx:ident, [$($previous_tx:ident),*] ) => {
            for (index, txin) in $tx.inner_tx().global.unsigned_tx.input.iter().enumerate() {
                let prevout = &txin.previous_output;
                $(
                    let previous_tx = &$previous_tx.inner_tx().global.unsigned_tx;
                    if previous_tx.txid() == prevout.txid {
                        let (prev_script, prev_value) =
                            previous_tx
                                .output
                                .get(prevout.vout as usize)
                                .and_then(|txo| Some((txo.script_pubkey.as_bytes(), txo.value)))
                                .expect("Refered output is inexistant");
                        bitcoinconsensus::verify(
                            prev_script,
                            prev_value,
                            $tx.as_bitcoin_serialized().expect("Serializing tx").as_slice(),
                            index,
                        ).expect("Libbitcoinconsensus error");
                        continue;
                    }
                )*
                panic!("Could not find output pointed by txin");
            }
        };
    }

    #[test]
    fn test_transaction_chain_satisfaction() {
        const CSV_VALUE: u32 = 42;

        let secp = bitcoin::secp256k1::Secp256k1::new();

        // Let's get the 10th key of each
        let child_number = bip32::ChildNumber::from(10);

        // Keys, keys, keys everywhere !
        let (
            (managers_priv, managers),
            (non_managers_priv, non_managers),
            (cosigners_priv, cosigners),
        ) = get_participants_sets(&secp);
        let all_participants_xpriv = managers_priv
            .iter()
            .chain(non_managers_priv.iter())
            .cloned()
            .collect::<Vec<bip32::ExtendedPrivKey>>();

        // Get the script descriptors for the txos we're going to create
        let unvault_descriptor = unvault_descriptor(
            non_managers.clone(),
            managers.clone(),
            cosigners.clone(),
            CSV_VALUE,
        )
        .expect("Unvault descriptor generation error")
        .derive(child_number);
        let cpfp_descriptor = unvault_cpfp_descriptor(managers.clone())
            .expect("Unvault CPFP descriptor generation error")
            .derive(child_number);
        let vault_descriptor = vault_descriptor(
            managers
                .into_iter()
                .chain(non_managers.into_iter())
                .collect::<Vec<DescriptorPublicKey>>(),
        )
        .expect("Vault descriptor generation error")
        .derive(child_number);

        // The funding transaction does not matter (random txid from my mempool)
        let vault_scriptpubkey = vault_descriptor.script_pubkey();
        let vault_raw_tx = Transaction {
            version: 2,
            lock_time: 0,
            input: vec![TxIn {
                previous_output: OutPoint::from_str(
                    "39a8212c6a9b467680d43e47b61b8363fe1febb761f9f548eb4a432b2bc9bbec:0",
                )
                .unwrap(),
                ..TxIn::default()
            }],
            output: vec![TxOut {
                value: 360,
                script_pubkey: vault_scriptpubkey.clone(),
            }],
        };
        let vault_txo = VaultTxOut::new(vault_raw_tx.output[0].value, &vault_descriptor);
        let vault_tx = VaultTransaction::new(Psbt::from_unsigned_tx(vault_raw_tx).unwrap());

        // The fee-bumping utxo, used in revaulting transactions inputs to bump their feerate.
        // We simulate a wallet utxo.
        let mut rng = SmallRng::from_entropy();
        let feebump_xpriv = get_random_privkey(&mut rng);
        let feebump_xpub = bip32::ExtendedPubKey::from_private(&secp, &feebump_xpriv);
        let feebump_descriptor =
            Descriptor::<DescriptorPublicKey>::Wpkh(DescriptorPublicKey::XPub(DescriptorXPub {
                origin: None,
                xpub: feebump_xpub,
                derivation_path: bip32::DerivationPath::from(vec![]),
                is_wildcard: false, // We are not going to derive from this one
            }));
        let raw_feebump_tx = Transaction {
            version: 2,
            lock_time: 0,
            input: vec![TxIn {
                previous_output: OutPoint::from_str(
                    "4bb4545bb4bc8853cb03e42984d677fbe880c81e7d95609360eed0d8f45b52f8:0",
                )
                .unwrap(),
                ..TxIn::default()
            }],
            output: vec![TxOut {
                value: 56730,
                script_pubkey: feebump_descriptor.script_pubkey(),
            }],
        };
        let feebump_txo = FeeBumpTxOut::new(raw_feebump_tx.output[0].clone());
        let feebump_tx = FeeBumpTransaction::new(Psbt::from_unsigned_tx(raw_feebump_tx).unwrap());

        // Create and sign the first (vault) emergency transaction
        let vault_txin = VaultTxIn::new(vault_tx.into_outpoint(0), vault_txo.clone(), RBF_SEQUENCE);
        let feebump_txin = FeeBumpTxIn::new(
            feebump_tx.into_outpoint(0),
            feebump_txo.clone(),
            RBF_SEQUENCE,
        );
        let emer_txo = EmergencyTxOut::new(TxOut {
            value: 450,
            ..TxOut::default()
        });
        let mut emergency_tx =
            EmergencyTransaction::new(vault_txin, Some(feebump_txin), emer_txo.clone(), 0);
        let emergency_tx_sighash_vault =
            emergency_tx.signature_hash(0, &vault_txo, &vault_descriptor.witness_script(), true);
        satisfy_transaction_input(
            &secp,
            &mut emergency_tx,
            0,
            &emergency_tx_sighash_vault,
            &all_participants_xpriv,
            Some(child_number),
            true,
        );

        let emergency_tx_sighash_feebump =
            emergency_tx.signature_hash(1, &feebump_txo, &feebump_descriptor.script_code(), false);
        satisfy_transaction_input(
            &secp,
            &mut emergency_tx,
            1,
            &emergency_tx_sighash_feebump,
            &vec![feebump_xpriv],
            None,
            false,
        );
        emergency_tx.finalize().unwrap();
        assert_libbitcoinconsensus_validity!(emergency_tx, [vault_tx, feebump_tx]);

        // Create but don't sign the unvaulting transaction until all revaulting transactions
        // are
        let vault_txin = VaultTxIn::new(vault_tx.into_outpoint(0), vault_txo.clone(), u32::MAX);
        let unvault_txo = UnvaultTxOut::new(7000, &unvault_descriptor);
        let cpfp_txo = CpfpTxOut::new(330, &cpfp_descriptor);
        let mut unvault_tx =
            UnvaultTransaction::new(vault_txin, unvault_txo.clone(), cpfp_txo.clone(), 0);

        // Create and sign the cancel transaction
        let unvault_txin = UnvaultTxIn::new(
            unvault_tx.into_outpoint(0),
            unvault_txo.clone(),
            RBF_SEQUENCE,
        );
        let feebump_txin = FeeBumpTxIn::new(
            feebump_tx.into_outpoint(0),
            feebump_txo.clone(),
            RBF_SEQUENCE,
        );
        let revault_txo = VaultTxOut::new(6700, &vault_descriptor);
        let mut cancel_tx =
            CancelTransaction::new(unvault_txin, Some(feebump_txin), revault_txo, 0);
        let cancel_tx_sighash =
            cancel_tx.signature_hash(0, &unvault_txo, &unvault_descriptor.witness_script(), true);
        satisfy_transaction_input(
            &secp,
            &mut cancel_tx,
            0,
            &cancel_tx_sighash,
            &all_participants_xpriv,
            Some(child_number),
            true,
        );
        let cancel_tx_sighash_feebump =
            cancel_tx.signature_hash(1, &feebump_txo, &feebump_descriptor.script_code(), false);

        satisfy_transaction_input(
            &secp,
            &mut cancel_tx,
            1,
            &cancel_tx_sighash_feebump,
            &vec![feebump_xpriv],
            None, // No derivation path for the feebump key
            false,
        );
        cancel_tx.finalize().unwrap();
        assert_libbitcoinconsensus_validity!(cancel_tx, [unvault_tx, feebump_tx]);

        // Create and sign the second (unvault) emergency transaction
        let unvault_txin = UnvaultTxIn::new(
            unvault_tx.into_outpoint(0),
            unvault_txo.clone(),
            RBF_SEQUENCE,
        );
        let feebump_txin = FeeBumpTxIn::new(
            feebump_tx.into_outpoint(0),
            feebump_txo.clone(),
            RBF_SEQUENCE,
        );
        let mut unemergency_tx =
            UnvaultEmergencyTransaction::new(unvault_txin, Some(feebump_txin), emer_txo, 0);
        let unemergency_tx_sighash = unemergency_tx.signature_hash(
            0,
            &unvault_txo,
            &unvault_descriptor.witness_script(),
            true,
        );
        satisfy_transaction_input(
            &secp,
            &mut unemergency_tx,
            0,
            &unemergency_tx_sighash,
            &all_participants_xpriv,
            Some(child_number),
            true,
        );
        // We don't have satisfied the feebump input yet!
        match unemergency_tx.finalize() {
            Err(e) => assert!(e
                .to_string()
                .contains("Could not find pubkey associated with hash")),
            Ok(_) => unreachable!(),
        }
        // If we don't satisfy the feebump input, libbitcoinconsensus will yell
        // uncommenting this should result in a failure:
        //assert_libbitcoinconsensus_validity!(unemergency_tx, [unvault_tx, feebump_tx]);

        // Now actually satisfy it, libbitcoinconsensus should not yell
        let unemer_tx_sighash_feebump = unemergency_tx.signature_hash(
            1,
            &feebump_txo,
            &feebump_descriptor.script_code(),
            false,
        );
        satisfy_transaction_input(
            &secp,
            &mut unemergency_tx,
            1,
            &unemer_tx_sighash_feebump,
            &vec![feebump_xpriv],
            None,
            false,
        );
        unemergency_tx
            .finalize()
            .expect("Finalizing the unvault emergency transaction");
        assert_libbitcoinconsensus_validity!(unemergency_tx, [unvault_tx, feebump_tx]);

        // Now we can sign the unvault
        let unvault_tx_sighash =
            unvault_tx.signature_hash(0, &vault_txo, &vault_descriptor.witness_script());
        satisfy_transaction_input(
            &secp,
            &mut unvault_tx,
            0,
            &unvault_tx_sighash,
            &all_participants_xpriv,
            Some(child_number),
            false,
        );
        unvault_tx.finalize().expect("Finalizing the unvault");
        assert_libbitcoinconsensus_validity!(unvault_tx, [vault_tx]);

        // FIXME: We should test batching as well for the spend transaction
        // Create and sign a spend transaction
        let unvault_txin = UnvaultTxIn::new(
            unvault_tx.into_outpoint(0),
            unvault_txo.clone(),
            CSV_VALUE - 1,
        );
        let spend_txo = ExternalTxOut::new(TxOut {
            value: 1,
            ..TxOut::default()
        });
        // Test satisfaction failure with a wrong CSV value
        let mut spend_tx = SpendTransaction::new(
            vec![unvault_txin],
            vec![SpendTxOut::Destination(spend_txo.clone())],
            0,
        );
        let spend_tx_sighash =
            spend_tx.signature_hash(0, &unvault_txo, &unvault_descriptor.witness_script());
        satisfy_transaction_input(
            &secp,
            &mut spend_tx,
            0,
            &spend_tx_sighash,
            &managers_priv
                .iter()
                .chain(cosigners_priv.iter())
                .copied()
                .collect::<Vec<bip32::ExtendedPrivKey>>(),
            Some(child_number),
            false,
        );
        match spend_tx.finalize() {
            Err(e) => assert!(e.to_string().contains("Input satisfaction error")),
            Ok(_) => unreachable!(),
        }

        // "This time for sure !"
        let unvault_txin = UnvaultTxIn::new(
            unvault_tx.into_outpoint(0),
            unvault_txo.clone(),
            CSV_VALUE, // The valid sequence this time
        );
        let mut spend_tx = SpendTransaction::new(
            vec![unvault_txin],
            vec![SpendTxOut::Destination(spend_txo.clone())],
            0,
        );
        let spend_tx_sighash =
            spend_tx.signature_hash(0, &unvault_txo, &unvault_descriptor.witness_script());
        satisfy_transaction_input(
            &secp,
            &mut spend_tx,
            0,
            &spend_tx_sighash,
            &managers_priv
                .iter()
                .chain(cosigners_priv.iter())
                .copied()
                .collect::<Vec<bip32::ExtendedPrivKey>>(),
            Some(child_number),
            false,
        );
        spend_tx.finalize().expect("Finalizing spend transaction");
        assert_libbitcoinconsensus_validity!(spend_tx, [unvault_tx]);

        // Test that we can get the hexadecimal representation of each transaction without error
        vault_tx.hex().expect("Hex repr vault_tx");
        unvault_tx.hex().expect("Hex repr unvault_tx");
        spend_tx.hex().expect("Hex repr spend_tx");
        cancel_tx.hex().expect("Hex repr cancel_tx");
        emergency_tx.hex().expect("Hex repr emergency_tx");
        feebump_tx.hex().expect("Hex repr feebump_tx");
    }
}
