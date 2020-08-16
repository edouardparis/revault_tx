///! Revault transactions
///!
///! Typesafe routines to create bare revault transactions.
///!
use super::revault_error::RevaultError;

use bitcoin::{
    consensus::encode,
    consensus::encode::{serialize, Encodable},
    OutPoint, PublicKey, Script, SigHash, SigHashType, Transaction, TxIn, TxOut,
};
use miniscript::{BitcoinSig, Descriptor, MiniscriptKey, Satisfier, ToPublicKey};
use secp256k1::Signature;

use std::collections::HashMap;
use std::io;

const RBF_SEQUENCE: u32 = u32::MAX - 2;

/// A transaction output created by a Revault transaction.
#[derive(PartialEq, Eq, Debug, Clone, Hash)]
pub enum RevaultTxOut {
    /// A vault transaction output. Used by the funding / deposit transactions, the cancel
    /// transactions, and the spend transactions (for the change).
    VaultTxOut(TxOut),
    /// *The* unvault transaction output.
    UnvaultTxOut(TxOut),
    /// A spend transaction output. As Revault is flexible by default with regard to the
    /// destination of the spend transaction funds, any number of these can be present in a spend
    /// transaction (use a VaultTxOut for the change output however).
    SpendTxOut(TxOut),
    /// The Emergency Deep Vault, the destination of the emergency transactions fund.
    EmergencyTxOut(TxOut),
    /// The "fee bumping" output, attached to the unvault transaction so that the fund managers can
    /// CPFP.
    CpfpTxOut(TxOut),
}

/// A transaction output spent by a Revault transaction.
#[derive(PartialEq, Eq, Debug, Copy, Clone, Hash, PartialOrd, Ord)]
pub enum RevaultPrevout {
    /// A vault txo spent by the unvault transaction and the emergency transaction.
    VaultPrevout(OutPoint),
    /// An unvault txo spent by the cancel transaction, an emergency transaction, and
    /// the spend transaction.
    UnvaultPrevout(OutPoint),
    /// A wallet txo spent by a revaulting (cancel, emergency) transaction to bump the
    /// transaction feerate.
    /// This output is often created by a first stage transaction, but may directly be a wallet
    /// utxo.
    FeeBumpPrevout(OutPoint),
    /// The unvault CPFP txo spent to accelerate the confirmation of the unvault transaction.
    CpfpPrevout(OutPoint),
}

// Using a struct wrapper around the enum wrapper to create an encapsulation behaviour would be
// quite verbose..

/// A Revault transaction. Apart from the VaultTransaction, all variants must be instanciated
/// using the new_*() methods.
#[derive(PartialEq, Eq, Debug)]
pub enum RevaultTransaction {
    /// The funding transaction, we don't create it but it's a handy wrapper.
    VaultTransaction(Transaction),
    /// The unvaulting transaction, spending a vault and being eventually spent by a spend
    /// transaction (if not revaulted).
    UnvaultTransaction(Transaction),
    /// The transaction spending the unvaulting transaction, paying to one or multiple
    /// externally-controlled addresses, and possibly to a new vault txo for the change.
    SpendTransaction(Transaction),
    /// The transaction "revaulting" a spend attempt, i.e. spending the unvaulting transaction back
    /// to a vault txo.
    CancelTransaction(Transaction),
    /// The transaction spending either a vault or unvault txo to The Emergency Deep Vault.
    EmergencyTransaction(Transaction),
}

impl RevaultTransaction {
    /// Create an unvault transaction.
    /// An unvault transaction always spends one vault txout and contains one CPFP txout in
    /// addition to the unvault one.
    pub fn new_unvault(
        prevouts: &[RevaultPrevout; 1],
        txouts: &[RevaultTxOut; 2],
    ) -> Result<Self, RevaultError> {
        match (prevouts, txouts) {
            (
                [RevaultPrevout::VaultPrevout(ref vault_prevout)],
                [RevaultTxOut::UnvaultTxOut(ref unvault_txout), RevaultTxOut::CpfpTxOut(ref cpfp_txout)],
            ) => {
                let vault_input = TxIn {
                    previous_output: *vault_prevout,
                    ..Default::default()
                };
                Ok(RevaultTransaction::UnvaultTransaction(Transaction {
                    version: 2,
                    lock_time: 0, // FIXME: anti fee snipping
                    input: vec![vault_input],
                    output: vec![unvault_txout.clone(), cpfp_txout.clone()],
                }))
            }
            _ => Err(RevaultError::TransactionCreation(format!(
                "Unvault: type mismatch on prevout ({:?}) or output(s) ({:?})",
                prevouts, txouts
            ))),
        }
    }

    /// Create a new spend transaction.
    /// A spend transaction can batch multiple unvault txouts, and may have any number of
    /// txouts (including, but not restricted to, change).
    pub fn new_spend(
        prevouts: &[RevaultPrevout],
        outputs: &[RevaultTxOut],
        csv_value: u32,
    ) -> Result<Self, RevaultError> {
        let mut txins = Vec::<TxIn>::with_capacity(prevouts.len());
        for prevout in prevouts {
            if let RevaultPrevout::UnvaultPrevout(ref prev) = prevout {
                txins.push(TxIn {
                    previous_output: *prev,
                    sequence: csv_value,
                    ..TxIn::default()
                })
            } else {
                return Err(RevaultError::TransactionCreation(format!(
                    "Spend: prevout ({:?}) type mismatch",
                    prevout
                )));
            }
        }

        let mut txouts = Vec::<TxOut>::with_capacity(outputs.len());
        for out in outputs {
            match out {
                RevaultTxOut::SpendTxOut(ref txout) | RevaultTxOut::VaultTxOut(ref txout) => {
                    txouts.push(txout.clone())
                }
                _ => {
                    return Err(RevaultError::TransactionCreation(format!(
                        "Spend: output ({:?}) type mismatch",
                        out
                    )))
                }
            }
        }

        Ok(RevaultTransaction::SpendTransaction(Transaction {
            version: 2,
            lock_time: 0,
            input: txins,
            output: txouts,
        }))
    }

    /// Create a new cancel transaction.
    /// A cancel transaction always pays to a vault output and spend the unvault output, and
    /// may have a fee-bumping input.
    pub fn new_cancel(
        prevouts: &[RevaultPrevout],
        txouts: &[RevaultTxOut],
    ) -> Result<RevaultTransaction, RevaultError> {
        match (prevouts, txouts) {
            // FIXME: Use https://github.com/rust-lang/rust/issues/54883 once stabilized ..
            (
                &[RevaultPrevout::UnvaultPrevout(_)],
                &[RevaultTxOut::VaultTxOut(ref vault_txout)],
            )
            | (
                &[RevaultPrevout::UnvaultPrevout(_), RevaultPrevout::FeeBumpPrevout(_)],
                &[RevaultTxOut::VaultTxOut(ref vault_txout)],
            ) => {
                let inputs = prevouts
                    .iter()
                    .map(|prevout| TxIn {
                        previous_output: match prevout {
                            RevaultPrevout::UnvaultPrevout(ref prev)
                            | RevaultPrevout::FeeBumpPrevout(ref prev) => *prev,
                            _ => unreachable!(),
                        },
                        sequence: RBF_SEQUENCE,
                        ..Default::default()
                    })
                    .collect();

                Ok(RevaultTransaction::CancelTransaction(Transaction {
                    version: 2,
                    lock_time: 0,
                    input: inputs,
                    output: vec![vault_txout.clone()],
                }))
            }
            _ => Err(RevaultError::TransactionCreation(format!(
                "Cancel: prevout(s) ({:?}) or output(s) ({:?}) type mismatch",
                prevouts, txouts,
            ))),
        }
    }

    /// Create an emergency transaction.
    /// There are two emergency transactions, one spending the vault output and one spending
    /// the unvault output. Both may have a fee-bumping input.
    pub fn new_emergency(
        prevouts: &[RevaultPrevout],
        txouts: &[RevaultTxOut],
    ) -> Result<RevaultTransaction, RevaultError> {
        // FIXME: Use https://github.com/rust-lang/rust/issues/54883 once stabilized ..
        match (prevouts, txouts) {
            (
                &[RevaultPrevout::VaultPrevout(_)],
                &[RevaultTxOut::EmergencyTxOut(ref emer_txout)],
            )
            | (
                &[RevaultPrevout::VaultPrevout(_), RevaultPrevout::FeeBumpPrevout(_)],
                &[RevaultTxOut::EmergencyTxOut(ref emer_txout)],
            )
            | (
                &[RevaultPrevout::UnvaultPrevout(_)],
                &[RevaultTxOut::EmergencyTxOut(ref emer_txout)],
            )
            | (
                &[RevaultPrevout::UnvaultPrevout(_), RevaultPrevout::FeeBumpPrevout(_)],
                &[RevaultTxOut::EmergencyTxOut(ref emer_txout)],
            ) => {
                let inputs = prevouts
                    .iter()
                    .map(|prevout| TxIn {
                        previous_output: match prevout {
                            RevaultPrevout::VaultPrevout(ref prev)
                            | RevaultPrevout::UnvaultPrevout(ref prev)
                            | RevaultPrevout::FeeBumpPrevout(ref prev) => *prev,
                            _ => unreachable!(),
                        },
                        sequence: RBF_SEQUENCE,
                        ..Default::default()
                    })
                    .collect();

                Ok(RevaultTransaction::EmergencyTransaction(Transaction {
                    version: 2,
                    lock_time: 0,
                    input: inputs,
                    output: vec![emer_txout.clone()],
                }))
            }
            _ => Err(RevaultError::TransactionCreation(format!(
                "Emergency: prevout(s) ({:?}) or output(s) ({:?}) type mismatch",
                prevouts, txouts,
            ))),
        }
    }

    /// Get the specified output of this transaction as an OutPoint to be referenced
    /// in a following transaction.
    /// Mainly useful to avoid the destructuring boilerplate.
    pub fn prevout(&self, vout: u32) -> OutPoint {
        match *self {
            RevaultTransaction::VaultTransaction(ref tx)
            | RevaultTransaction::UnvaultTransaction(ref tx)
            | RevaultTransaction::SpendTransaction(ref tx)
            | RevaultTransaction::CancelTransaction(ref tx)
            | RevaultTransaction::EmergencyTransaction(ref tx) => OutPoint {
                txid: tx.txid(),
                vout,
            },
        }
    }

    /// Get the sighash for any RevaultTransaction input.
    /// This is a wrapper around rust-bitcoin's `signature_hash()` but as we only ever sign
    /// transaction with ALL or ALL|ANYONECANPAY we don't need to be generalistic with choosing
    /// the type.
    pub fn signature_hash(
        &self,
        input_index: usize,
        script_pubkey: &Script,
        anyonecanpay: bool,
    ) -> SigHash {
        match *self {
            RevaultTransaction::VaultTransaction(ref tx)
            | RevaultTransaction::UnvaultTransaction(ref tx)
            | RevaultTransaction::SpendTransaction(ref tx)
            | RevaultTransaction::CancelTransaction(ref tx)
            | RevaultTransaction::EmergencyTransaction(ref tx) => {
                if anyonecanpay {
                    return tx.signature_hash(input_index, script_pubkey, 0x81);
                }
                tx.signature_hash(input_index, script_pubkey, 0x01)
            }
        }
    }

    /// Get the hexadecimal representation of the transaction as used by the bitcoind API.
    pub fn hex(&self) -> String {
        let mut buff = Vec::<u8>::new();
        let mut as_hex = String::new();

        self.consensus_encode(&mut buff);
        for byte in buff.into_iter() {
            as_hex.push_str(&format!("{:02x}", byte));
        }

        as_hex
    }
}

impl Encodable for RevaultTransaction {
    fn consensus_encode<S: io::Write>(&self, mut s: S) -> Result<usize, encode::Error> {
        match *self {
            RevaultTransaction::VaultTransaction(ref tx)
            | RevaultTransaction::UnvaultTransaction(ref tx)
            | RevaultTransaction::SpendTransaction(ref tx)
            | RevaultTransaction::CancelTransaction(ref tx)
            | RevaultTransaction::EmergencyTransaction(ref tx) => tx.consensus_encode(&mut s),
        }
    }
}

/// A small wrapper around what is needed to implement the Satisfier trait for Revault
/// transactions.
struct RevaultInputSatisfier<Pk: MiniscriptKey> {
    pkhashmap: HashMap<Pk::Hash, Pk>,
    sigmap: HashMap<Pk, BitcoinSig>,
    sequence: u32,
}

impl<Pk: MiniscriptKey + ToPublicKey> RevaultInputSatisfier<Pk> {
    fn new(sequence: u32) -> RevaultInputSatisfier<Pk> {
        RevaultInputSatisfier::<Pk> {
            sequence,
            pkhashmap: HashMap::<Pk::Hash, Pk>::new(),
            sigmap: HashMap::<Pk, BitcoinSig>::new(),
        }
    }

    fn insert_sig(
        &mut self,
        pubkey: Pk,
        sig: Signature,
        is_anyonecanpay: bool,
    ) -> Option<BitcoinSig> {
        self.pkhashmap
            .insert(pubkey.to_pubkeyhash(), pubkey.clone());
        self.sigmap.insert(
            pubkey,
            (
                sig,
                if is_anyonecanpay {
                    SigHashType::AllPlusAnyoneCanPay
                } else {
                    SigHashType::All
                },
            ),
        )
    }
}

impl<Pk: MiniscriptKey + ToPublicKey> Satisfier<Pk> for RevaultInputSatisfier<Pk> {
    fn lookup_sig(&self, key: &Pk) -> Option<BitcoinSig> {
        self.sigmap.get(key).copied()
    }

    // The policy compiler will often optimize the Script to use pkH, so we need this method to be
    // implemented *both* for satisfaction and disatisfaction !
    fn lookup_pkh_sig(&self, keyhash: &Pk::Hash) -> Option<(PublicKey, BitcoinSig)> {
        if let Some(key) = self.pkhashmap.get(keyhash) {
            if let Some((sig, sig_type)) = self.lookup_sig(key) {
                return Some((key.to_public_key(), (sig, sig_type)));
            }
        }
        None
    }

    fn check_after(&self, csv: u32) -> bool {
        self.sequence == csv
    }
}

/// A wrapper handling the satisfaction of a RevaultTransaction input given the input's index
/// and the previous output's script descriptor
pub struct RevaultSatisfier<'a, Pk: MiniscriptKey + ToPublicKey> {
    txin: &'a mut TxIn,
    descriptor: &'a Descriptor<Pk>,
    satisfier: RevaultInputSatisfier<Pk>,
}

impl<'a, Pk: MiniscriptKey + ToPublicKey> RevaultSatisfier<'a, Pk> {
    /// Create a satisfier for a RevaultTransaction from the actual transaction, the input's index,
    /// and the descriptor of the output spent by this input.
    /// Errors on OOB.
    pub fn new(
        transaction: &'a mut RevaultTransaction,
        input_index: usize,
        descriptor: &'a Descriptor<Pk>,
    ) -> Result<Self, RevaultError> {
        let txin = match transaction {
            RevaultTransaction::VaultTransaction(ref mut tx)
            | RevaultTransaction::UnvaultTransaction(ref mut tx)
            | RevaultTransaction::SpendTransaction(ref mut tx)
            | RevaultTransaction::CancelTransaction(ref mut tx)
            | RevaultTransaction::EmergencyTransaction(ref mut tx) => {
                if input_index >= tx.input.len() {
                    return Err(RevaultError::InputSatisfaction(format!(
                        "Input index '{}' out of bonds of the transaction '{:?}'.",
                        input_index, tx.input
                    )));
                }
                &mut tx.input[input_index]
            }
        };

        Ok(Self {
            satisfier: RevaultInputSatisfier::new(txin.sequence),
            txin,
            descriptor,
        })
    }

    /// Insert a signature for a given pubkey to eventually satisfy the spending conditions of the
    /// referenced utxo.
    /// This is a wrapper around the mapping from a public key to signature used by the Miniscript
    /// satisfier, and as we only ever use ALL or ALL|ANYONECANPAY signatures, this restrics the
    /// signature type using a boolean.
    pub fn insert_sig(
        &mut self,
        pubkey: Pk,
        sig: Signature,
        is_anyonecanpay: bool,
    ) -> Option<BitcoinSig> {
        self.satisfier.insert_sig(pubkey, sig, is_anyonecanpay)
    }

    /// Fulfill the txin's witness. Errors if we can't provide a valid one out of the previously
    /// given signatures.
    pub fn satisfy(&mut self) -> Result<(), RevaultError> {
        if let Err(e) = self.descriptor.satisfy(&mut self.txin, &self.satisfier) {
            return Err(RevaultError::InputSatisfaction(format!(
                "Script satisfaction error: {}.",
                e
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            get_default_unvault_descriptors, get_default_vault_descriptors, unvault_cpfp_descriptor,
        },
        RevaultError, RevaultPrevout, RevaultSatisfier, RevaultTransaction, RevaultTxOut,
        RBF_SEQUENCE,
    };

    use rand::RngCore;
    use std::str::FromStr;

    use bitcoin::{OutPoint, PublicKey, Transaction, TxIn, TxOut};

    fn get_random_privkey() -> secp256k1::SecretKey {
        let mut rand_bytes = [0u8; 32];
        let mut secret_key = Err(secp256k1::Error::InvalidSecretKey);

        while secret_key.is_err() {
            rand::thread_rng().fill_bytes(&mut rand_bytes);
            secret_key = secp256k1::SecretKey::from_slice(&rand_bytes);
        }

        secret_key.unwrap()
    }

    #[test]
    fn test_transaction_creation() {
        // Transactions which happened to be in my mempool
        let outpoint = OutPoint::from_str(
            "ea4a9f84cce4e5b195b496e2823f7939b474f3fd3d2d8d59b91bb2312a8113f3:0",
        )
        .unwrap();
        let feebump_outpoint = OutPoint::from_str(
            "1d239c9299a7e350e3ae6e5fb4068f13b4e01fe188a0d0533f6555aad6b17b0a:0",
        )
        .unwrap();

        let vault_prevout = RevaultPrevout::VaultPrevout(outpoint);
        let unvault_prevout = RevaultPrevout::UnvaultPrevout(outpoint);
        let feebump_prevout = RevaultPrevout::FeeBumpPrevout(feebump_outpoint);

        let txout = TxOut {
            value: 18,
            ..TxOut::default()
        };
        let unvault_txout = RevaultTxOut::UnvaultTxOut(txout.clone());
        let feebump_txout = RevaultTxOut::CpfpTxOut(txout.clone());
        let spend_txout = RevaultTxOut::SpendTxOut(txout.clone());
        let vault_txout = RevaultTxOut::VaultTxOut(txout.clone());
        let emer_txout = RevaultTxOut::EmergencyTxOut(txout.clone());

        // =======================
        // The unvault transaction
        assert_eq!(
            RevaultTransaction::new_unvault(
                &[vault_prevout],
                &[unvault_txout.clone(), feebump_txout.clone()]
            ),
            Ok(RevaultTransaction::UnvaultTransaction(Transaction {
                version: 2,
                lock_time: 0,
                input: vec![TxIn {
                    previous_output: outpoint,
                    ..TxIn::default()
                }],
                output: vec![txout.clone(), txout.clone()]
            }))
        );
        assert_eq!(
            RevaultTransaction::new_unvault(
                &[vault_prevout],
                &[vault_txout.clone(), feebump_txout.clone()]
            ),
            Err(RevaultError::TransactionCreation(format!(
                "Unvault: type mismatch on prevout ({:?}) or output(s) ({:?})",
                &[vault_prevout],
                &[vault_txout.clone(), feebump_txout.clone()]
            )))
        );

        // =====================
        // The spend transaction
        assert_eq!(
            RevaultTransaction::new_spend(&[unvault_prevout], &[spend_txout.clone()], 22),
            Ok(RevaultTransaction::SpendTransaction(Transaction {
                version: 2,
                lock_time: 0,
                input: vec![TxIn {
                    previous_output: outpoint,
                    sequence: 22,
                    ..TxIn::default()
                }],
                output: vec![txout.clone()]
            }))
        );
        assert_eq!(
            RevaultTransaction::new_spend(&[vault_prevout], &[spend_txout.clone()], 144),
            Err(RevaultError::TransactionCreation(format!(
                "Spend: prevout ({:?}) type mismatch",
                vault_prevout,
            )))
        );
        // multiple inputs
        assert_eq!(
            RevaultTransaction::new_spend(
                &[unvault_prevout, unvault_prevout],
                &[spend_txout.clone()],
                9
            ),
            Ok(RevaultTransaction::SpendTransaction(Transaction {
                version: 2,
                lock_time: 0,
                input: vec![
                    TxIn {
                        previous_output: outpoint,
                        sequence: 9,
                        ..TxIn::default()
                    },
                    TxIn {
                        previous_output: outpoint,
                        sequence: 9,
                        ..TxIn::default()
                    }
                ],
                output: vec![txout.clone()]
            }))
        );
        assert_eq!(
            RevaultTransaction::new_spend(
                &[unvault_prevout, feebump_prevout],
                &[spend_txout.clone()],
                144
            ),
            Err(RevaultError::TransactionCreation(format!(
                "Spend: prevout ({:?}) type mismatch",
                feebump_prevout,
            )))
        );

        // multiple outputs
        assert_eq!(
            RevaultTransaction::new_spend(
                &[unvault_prevout],
                &[spend_txout.clone(), spend_txout.clone()],
                24
            ),
            Ok(RevaultTransaction::SpendTransaction(Transaction {
                version: 2,
                lock_time: 0,
                input: vec![TxIn {
                    previous_output: outpoint,
                    sequence: 24,
                    ..TxIn::default()
                }],
                output: vec![txout.clone(), txout.clone()]
            }))
        );

        // Both (with one output being change)
        assert_eq!(
            RevaultTransaction::new_spend(
                &[unvault_prevout, unvault_prevout],
                &[spend_txout.clone(), vault_txout.clone()],
                24
            ),
            Ok(RevaultTransaction::SpendTransaction(Transaction {
                version: 2,
                lock_time: 0,
                input: vec![
                    TxIn {
                        previous_output: outpoint,
                        sequence: 24,
                        ..TxIn::default()
                    },
                    TxIn {
                        previous_output: outpoint,
                        sequence: 24,
                        ..TxIn::default()
                    }
                ],
                output: vec![txout.clone(), txout.clone()]
            }))
        );

        // =====================
        // The cancel transaction
        // Without feebump
        assert_eq!(
            RevaultTransaction::new_cancel(&[unvault_prevout], &[vault_txout.clone()]),
            Ok(RevaultTransaction::CancelTransaction(Transaction {
                version: 2,
                lock_time: 0,
                input: vec![TxIn {
                    previous_output: outpoint,
                    sequence: RBF_SEQUENCE,
                    ..TxIn::default()
                }],
                output: vec![txout.clone()]
            }))
        );
        assert_eq!(
            RevaultTransaction::new_cancel(
                &[unvault_prevout],
                &[vault_txout.clone(), vault_txout.clone()]
            ),
            Err(RevaultError::TransactionCreation(format!(
                "Cancel: prevout(s) ({:?}) or output(s) ({:?}) type mismatch",
                &[unvault_prevout],
                &[vault_txout.clone(), vault_txout.clone()]
            )))
        );

        // With feebump
        assert_eq!(
            RevaultTransaction::new_cancel(
                &[unvault_prevout, feebump_prevout],
                &[vault_txout.clone()],
            ),
            Ok(RevaultTransaction::CancelTransaction(Transaction {
                version: 2,
                lock_time: 0,
                input: vec![
                    TxIn {
                        previous_output: outpoint,
                        sequence: RBF_SEQUENCE,
                        ..TxIn::default()
                    },
                    TxIn {
                        previous_output: feebump_outpoint,
                        sequence: RBF_SEQUENCE,
                        ..TxIn::default()
                    }
                ],
                output: vec![txout.clone()]
            }))
        );
        assert_eq!(
            RevaultTransaction::new_cancel(
                &[unvault_prevout, feebump_prevout],
                &[vault_txout.clone(), vault_txout.clone()]
            ),
            Err(RevaultError::TransactionCreation(format!(
                "Cancel: prevout(s) ({:?}) or output(s) ({:?}) type mismatch",
                &[unvault_prevout, feebump_prevout],
                &[vault_txout.clone(), vault_txout.clone()]
            )))
        );

        // =====================
        // The emergency transactions
        // Vault emergency, without feebump
        assert_eq!(
            RevaultTransaction::new_emergency(&[vault_prevout], &[emer_txout.clone()]),
            Ok(RevaultTransaction::EmergencyTransaction(Transaction {
                version: 2,
                lock_time: 0,
                input: vec![TxIn {
                    previous_output: outpoint,
                    sequence: RBF_SEQUENCE,
                    ..TxIn::default()
                }],
                output: vec![txout.clone()]
            }))
        );
        assert_eq!(
            RevaultTransaction::new_emergency(&[vault_prevout], &[vault_txout.clone()]),
            Err(RevaultError::TransactionCreation(format!(
                "Emergency: prevout(s) ({:?}) or output(s) ({:?}) type mismatch",
                &[vault_prevout],
                &[vault_txout.clone()]
            )))
        );

        // Vault emergency, with feebump
        assert_eq!(
            RevaultTransaction::new_emergency(
                &[vault_prevout, feebump_prevout],
                &[emer_txout.clone()],
            ),
            Ok(RevaultTransaction::EmergencyTransaction(Transaction {
                version: 2,
                lock_time: 0,
                input: vec![
                    TxIn {
                        previous_output: outpoint,
                        sequence: RBF_SEQUENCE,
                        ..TxIn::default()
                    },
                    TxIn {
                        previous_output: feebump_outpoint,
                        sequence: RBF_SEQUENCE,
                        ..TxIn::default()
                    }
                ],
                output: vec![txout.clone()]
            }))
        );
        assert_eq!(
            RevaultTransaction::new_emergency(
                &[vault_prevout, vault_prevout],
                &[emer_txout.clone()]
            ),
            Err(RevaultError::TransactionCreation(format!(
                "Emergency: prevout(s) ({:?}) or output(s) ({:?}) type mismatch",
                &[vault_prevout, vault_prevout],
                &[emer_txout.clone()]
            )))
        );

        // Unvault emergency, without feebump
        assert_eq!(
            RevaultTransaction::new_emergency(&[unvault_prevout], &[emer_txout.clone()]),
            Ok(RevaultTransaction::EmergencyTransaction(Transaction {
                version: 2,
                lock_time: 0,
                input: vec![TxIn {
                    previous_output: outpoint,
                    sequence: RBF_SEQUENCE,
                    ..TxIn::default()
                }],
                output: vec![txout.clone()]
            }))
        );
        assert_eq!(
            RevaultTransaction::new_emergency(&[unvault_prevout], &[spend_txout.clone()]),
            Err(RevaultError::TransactionCreation(format!(
                "Emergency: prevout(s) ({:?}) or output(s) ({:?}) type mismatch",
                &[unvault_prevout],
                &[spend_txout.clone()]
            )))
        );

        // Unvault emergency, with feebump
        assert_eq!(
            RevaultTransaction::new_emergency(
                &[unvault_prevout, feebump_prevout],
                &[emer_txout.clone()],
            ),
            Ok(RevaultTransaction::EmergencyTransaction(Transaction {
                version: 2,
                lock_time: 0,
                input: vec![
                    TxIn {
                        previous_output: outpoint,
                        sequence: RBF_SEQUENCE,
                        ..TxIn::default()
                    },
                    TxIn {
                        previous_output: feebump_outpoint,
                        sequence: RBF_SEQUENCE,
                        ..TxIn::default()
                    }
                ],
                output: vec![txout.clone()]
            }))
        );
        assert_eq!(
            RevaultTransaction::new_emergency(
                &[unvault_prevout, vault_prevout],
                &[emer_txout.clone()]
            ),
            Err(RevaultError::TransactionCreation(format!(
                "Emergency: prevout(s) ({:?}) or output(s) ({:?}) type mismatch",
                &[unvault_prevout, vault_prevout],
                &[emer_txout.clone()]
            )))
        );
    }

    #[test]
    fn test_transaction_chain_satisfaction() {
        let secp = secp256k1::Secp256k1::new();

        // Generate some private key pairs for every participant
        let managers_priv = (0..3)
            .map(|_| get_random_privkey())
            .collect::<Vec<secp256k1::SecretKey>>();
        let managers = managers_priv
            .iter()
            .map(|privkey| PublicKey {
                compressed: true,
                key: secp256k1::PublicKey::from_secret_key(&secp, &privkey),
            })
            .collect::<Vec<PublicKey>>();
        let non_managers_priv = (0..8)
            .map(|_| get_random_privkey())
            .collect::<Vec<secp256k1::SecretKey>>();
        let non_managers = non_managers_priv
            .iter()
            .map(|privkey| PublicKey {
                compressed: true,
                key: secp256k1::PublicKey::from_secret_key(&secp, &privkey),
            })
            .collect::<Vec<PublicKey>>();
        let cosigners_priv = (0..8)
            .map(|_| get_random_privkey())
            .collect::<Vec<secp256k1::SecretKey>>();
        let cosigners = cosigners_priv
            .iter()
            .map(|privkey| PublicKey {
                compressed: true,
                key: secp256k1::PublicKey::from_secret_key(&secp, &privkey),
            })
            .collect::<Vec<PublicKey>>();
        let all_participants_priv = managers_priv
            .iter()
            .chain(non_managers_priv.iter())
            .cloned()
            .collect::<Vec<secp256k1::SecretKey>>();

        // Get the script descriptors for the txo we're going to create
        let unvault_descriptor =
            get_default_unvault_descriptors(&non_managers, &managers, &cosigners, CSV_VALUE)
                .expect("Unvault descriptor generation error");
        let cpfp_descriptor =
            unvault_cpfp_descriptor(&managers).expect("Unvault CPFP descriptor generation error");
        let vault_descriptor = get_default_vault_descriptors(
            &managers
                .into_iter()
                .chain(non_managers.into_iter())
                .collect::<Vec<PublicKey>>(),
        )
        .expect("Vault descriptor generation error");

        // The funding transaction does not matter (random txid from my mempool)
        let vault_scriptpubkey = vault_descriptor.script_pubkey();
        let vault_tx = RevaultTransaction::VaultTransaction(Transaction {
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
                value: 1,
                script_pubkey: vault_scriptpubkey.clone(),
            }],
        });
        let vault_prevout = RevaultPrevout::VaultPrevout(vault_tx.prevout(0));

        // Create and sign the first (vault) emergency transaction
        let emer_txo = RevaultTxOut::EmergencyTxOut(TxOut {
            value: 1,
            ..TxOut::default()
        });
        let mut emergency_tx =
            RevaultTransaction::new_emergency(&[vault_prevout], &[emer_txo.clone()])
                .expect("Vault emergency transaction creation falure");
        let emergency_tx_sighash = emergency_tx.signature_hash(0, &vault_scriptpubkey, true);
        let mut revault_sat = RevaultSatisfier::new(&mut emergency_tx, 0, &vault_descriptor)
            .expect("Creating satisfier.");
        all_participants_priv.iter().for_each(|privkey| {
            revault_sat.insert_sig(
                PublicKey {
                    compressed: true,
                    key: secp256k1::PublicKey::from_secret_key(&secp, &privkey),
                },
                secp.sign(
                    &secp256k1::Message::from_slice(&emergency_tx_sighash).unwrap(),
                    &privkey,
                ),
                true,
            );
        });
        revault_sat
            .satisfy()
            .expect("Satisfying emergency transaction");

        // Create but *do not sign* the unvaulting transaction until all revaulting transactions
        // are
        let (unvault_scriptpubkey, cpfp_scriptpubkey) = (
            unvault_descriptor.script_pubkey(),
            cpfp_descriptor.script_pubkey(),
        );
        let unvault_txo = RevaultTxOut::UnvaultTxOut(TxOut {
            value: 1,
            script_pubkey: unvault_scriptpubkey.clone(),
        });
        let cpfp_txo = RevaultTxOut::CpfpTxOut(TxOut {
            value: 330,
            script_pubkey: cpfp_scriptpubkey,
        });
        let mut unvault_tx =
            RevaultTransaction::new_unvault(&[vault_prevout], &[unvault_txo, cpfp_txo])
                .expect("Unvault transaction creation failure");

        // Create and sign the cancel transaction
        let unvault_prevout = RevaultPrevout::UnvaultPrevout(unvault_tx.prevout(0));
        let revault_txo = RevaultTxOut::VaultTxOut(TxOut {
            value: 1,
            script_pubkey: vault_descriptor.script_pubkey(),
        });
        let mut cancel_tx = RevaultTransaction::new_cancel(&[unvault_prevout], &[revault_txo])
            .expect("Cancel transaction creation failure");
        let cancel_tx_sighash = cancel_tx.signature_hash(0, &unvault_scriptpubkey, true);
        let mut revault_sat: RevaultSatisfier<PublicKey> =
            RevaultSatisfier::<PublicKey>::new(&mut cancel_tx, 0, &unvault_descriptor)
                .expect("Creating satisfier.");
        all_participants_priv.iter().for_each(|privkey| {
            revault_sat.insert_sig(
                PublicKey {
                    compressed: true,
                    key: secp256k1::PublicKey::from_secret_key(&secp, &privkey),
                },
                secp.sign(
                    &secp256k1::Message::from_slice(&cancel_tx_sighash).unwrap(),
                    &privkey,
                ),
                true,
            );
        });
        revault_sat
            .satisfy()
            .expect("Satisfying cancel transaction");

        // Create and sign the second (unvault) emergency transaction
        let mut unemergency_tx = RevaultTransaction::new_emergency(&[unvault_prevout], &[emer_txo])
            .expect("Unvault emergency transaction creation failure");
        let unemergency_tx_sighash = unemergency_tx.signature_hash(0, &unvault_scriptpubkey, true);
        revault_sat =
            RevaultSatisfier::<PublicKey>::new(&mut unemergency_tx, 0, &unvault_descriptor)
                .expect("Creating satisfier.");
        all_participants_priv.iter().for_each(|privkey| {
            revault_sat.insert_sig(
                PublicKey {
                    compressed: true,
                    key: secp256k1::PublicKey::from_secret_key(&secp, &privkey),
                },
                secp.sign(
                    &secp256k1::Message::from_slice(&unemergency_tx_sighash).unwrap(),
                    &privkey,
                ),
                true,
            );
        });
        revault_sat
            .satisfy()
            .expect("Satisfying unvault emergency transaction");

        // Now we can sign the unvault
        let unvault_tx_sighash = unvault_tx.signature_hash(0, &vault_scriptpubkey, false);
        revault_sat = RevaultSatisfier::<PublicKey>::new(&mut unvault_tx, 0, &unvault_descriptor)
            .expect("Creating satisfier.");
        all_participants_priv.iter().for_each(|privkey| {
            revault_sat.insert_sig(
                PublicKey {
                    compressed: true,
                    key: secp256k1::PublicKey::from_secret_key(&secp, &privkey),
                },
                secp.sign(
                    &secp256k1::Message::from_slice(&unvault_tx_sighash).unwrap(),
                    &privkey,
                ),
                false,
            );
        });
        revault_sat
            .satisfy()
            .expect("Satisfying unvault transaction");

        let spend_txo = RevaultTxOut::SpendTxOut(TxOut {
            value: 1,
            ..TxOut::default()
        });
        // Test satisfaction failure with a wrong CSV value
        {
            let mut spend_tx = RevaultTransaction::new_spend(
                &[unvault_prevout],
                &[spend_txo.clone()],
                CSV_VALUE - 1,
            )
            .expect("Spend transaction (n.1) creation failure");
            let spend_tx_sighash = spend_tx.signature_hash(0, &unvault_scriptpubkey, false);
            let mut tmp_revault_sat =
                RevaultSatisfier::<PublicKey>::new(&mut spend_tx, 0, &unvault_descriptor)
                    .expect("Creating satisfier.");
            // Only the managers + automated cosigners are required
            managers_priv
                .iter()
                .chain(cosigners_priv.iter())
                .for_each(|privkey| {
                    tmp_revault_sat.insert_sig(
                        PublicKey {
                            compressed: true,
                            key: secp256k1::PublicKey::from_secret_key(&secp, &privkey),
                        },
                        secp.sign(
                            &secp256k1::Message::from_slice(&spend_tx_sighash).unwrap(),
                            &privkey,
                        ),
                        false,
                    );
                });
            assert_eq!(
                tmp_revault_sat.satisfy(),
                Err(RevaultError::InputSatisfaction(
                    "Script satisfaction error: could not satisfy.".to_string()
                ))
            );
        }

        // "This time for sure !"
        let mut spend_tx =
            RevaultTransaction::new_spend(&[unvault_prevout], &[spend_txo], CSV_VALUE)
                .expect("Spend transaction (n.2) creation failure");
        let spend_tx_sighash = spend_tx.signature_hash(0, &unvault_scriptpubkey, false);
        revault_sat = RevaultSatisfier::<PublicKey>::new(&mut spend_tx, 0, &unvault_descriptor)
            .expect("Creating satisfier.");
        // Only the managers + automated cosigners are required
        managers_priv
            .iter()
            .chain(cosigners_priv.iter())
            .for_each(|privkey| {
                revault_sat.insert_sig(
                    PublicKey {
                        compressed: true,
                        key: secp256k1::PublicKey::from_secret_key(&secp, &privkey),
                    },
                    secp.sign(
                        &secp256k1::Message::from_slice(&spend_tx_sighash).unwrap(),
                        &privkey,
                    ),
                    false,
                );
            });
        revault_sat
            .satisfy()
            .expect("Satisfying the valid spend tx");
    }
}
