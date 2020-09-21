// Copyright 2019 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

use crate::{bytes::Bytes, hash::keccak};
use cfx_types::{Address, BigEndianHash, H160, H256, U256};
use keylib::{
    self, public_to_address, recover, verify_public, Public, Secret, Signature,
};
use malloc_size_of::{MallocSizeOf, MallocSizeOfOps};
use rlp::{self, Decodable, DecoderError, Encodable, Rlp, RlpStream};
use rlp_derive::{RlpDecodable, RlpEncodable};
use serde::{Deserialize, Serialize};
use std::{error, fmt, ops::Deref};
use unexpected::OutOfBounds;

/// Fake address for unsigned transactions.
pub const UNSIGNED_SENDER: Address = H160([0xff; 20]);

/// Shorter id for transactions in compact blocks
// TODO should be u48
pub type TxShortId = u64;

pub type TxPropagateId = u32;

// FIXME: Most errors here are bounded for TransactionPool and intended for rpc,
// FIXME: however these are unused, they are not errors for transaction itself.
// FIXME: Transaction verification and consensus related error can be separated.
#[derive(Debug, PartialEq, Clone)]
/// Errors concerning transaction processing.
pub enum TransactionError {
    /// Transaction is already imported to the queue
    AlreadyImported,
    /// Chain id in the transaction doesn't match the chain id of the network.
    ChainIdMismatch { expected: u32, got: u32 },
    /// Epoch height out of bound.
    EpochHeightOutOfBound {
        block_height: u64,
        set: u64,
        transaction_epoch_bound: u64,
    },
    /// The gas paid for transaction is lower than base gas.
    NotEnoughBaseGas {
        /// Absolute minimum gas required.
        required: U256,
        /// Gas provided.
        got: U256,
    },
    /// Transaction is not valid anymore (state already has higher nonce)
    Stale,
    /// Transaction has too low fee
    /// (there is already a transaction with the same sender-nonce but higher
    /// gas price)
    TooCheapToReplace,
    /// Transaction was not imported to the queue because limit has been
    /// reached.
    LimitReached,
    /// Transaction's gas price is below threshold.
    InsufficientGasPrice {
        /// Minimal expected gas price
        minimal: U256,
        /// Transaction gas price
        got: U256,
    },
    /// Transaction's gas is below currently set minimal gas requirement.
    InsufficientGas {
        /// Minimal expected gas
        minimal: U256,
        /// Transaction gas
        got: U256,
    },
    /// Sender doesn't have enough funds to pay for this transaction
    InsufficientBalance {
        /// Senders balance
        balance: U256,
        /// Transaction cost
        cost: U256,
    },
    /// Transactions gas is higher then current gas limit
    GasLimitExceeded {
        /// Current gas limit
        limit: U256,
        /// Declared transaction gas
        got: U256,
    },
    /// Transaction's gas limit (aka gas) is invalid.
    InvalidGasLimit(OutOfBounds<U256>),
    /// Signature error
    InvalidSignature(String),
    /// Transaction too big
    TooBig,
    /// Invalid RLP encoding
    InvalidRlp(String),
}

impl From<keylib::Error> for TransactionError {
    fn from(err: keylib::Error) -> Self {
        TransactionError::InvalidSignature(format!("{}", err))
    }
}

impl From<rlp::DecoderError> for TransactionError {
    fn from(err: rlp::DecoderError) -> Self {
        TransactionError::InvalidRlp(format!("{}", err))
    }
}

impl fmt::Display for TransactionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::TransactionError::*;
        let msg = match *self {
            AlreadyImported => "Already imported".into(),
            ChainIdMismatch { expected, got } => {
                format!("Chain id mismatch, expected {}, got {}", expected, got)
            }
            EpochHeightOutOfBound {
                block_height,
                set,
                transaction_epoch_bound,
            } => format!(
                "EpochHeight out of bound:\
                 block_height {}, transaction epoch_height {}, transaction_epoch_bound {}",
                block_height, set, transaction_epoch_bound
            ),
            NotEnoughBaseGas { got, required } => format!(
                "Transaction gas {} less than intrinsic gas {}",
                got, required
            ),
            Stale => "No longer valid".into(),
            TooCheapToReplace => "Gas price too low to replace".into(),
            LimitReached => "Transaction limit reached".into(),
            InsufficientGasPrice { minimal, got } => format!(
                "Insufficient gas price. Min={}, Given={}",
                minimal, got
            ),
            InsufficientGas { minimal, got } => {
                format!("Insufficient gas. Min={}, Given={}", minimal, got)
            }
            InsufficientBalance { balance, cost } => format!(
                "Insufficient balance for transaction. Balance={}, Cost={}",
                balance, cost
            ),
            GasLimitExceeded { limit, got } => {
                format!("Gas limit exceeded. Limit={}, Given={}", limit, got)
            }
            InvalidGasLimit(ref err) => format!("Invalid gas limit. {}", err),
            InvalidSignature(ref err) => {
                format!("Transaction has invalid signature: {}.", err)
            }
            TooBig => "Transaction too big".into(),
            InvalidRlp(ref err) => {
                format!("Transaction has invalid RLP structure: {}.", err)
            }
        };

        f.write_fmt(format_args!("Transaction error ({})", msg))
    }
}

impl error::Error for TransactionError {
    fn description(&self) -> &str { "Transaction error" }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// Create creates new contract.
    Create,
    /// Calls contract at given address.
    /// In the case of a transfer, this is the receiver's address.'
    Call(Address),
}

impl Default for Action {
    fn default() -> Action { Action::Create }
}

impl Decodable for Action {
    fn decode(rlp: &Rlp) -> Result<Self, DecoderError> {
        if rlp.is_empty() {
            Ok(Action::Create)
        } else {
            Ok(Action::Call(rlp.as_val()?))
        }
    }
}

impl Encodable for Action {
    fn rlp_append(&self, stream: &mut RlpStream) {
        match *self {
            Action::Create => stream.append_internal(&""),
            Action::Call(ref address) => stream.append_internal(address),
        };
    }
}

/// The parameters needed to determine the chain_id based on epoch_number.
#[derive(Clone, Debug, Eq, RlpEncodable, RlpDecodable, PartialEq, Default)]
pub struct ChainIdParams {
    /// Preconfigured chain_id.
    pub chain_id: u32,
}

impl ChainIdParams {
    /// The function return the chain_id with given parameters
    pub fn get_chain_id(&self, _epoch_number: u64) -> u32 { self.chain_id }
}

#[derive(
    Default,
    Debug,
    Clone,
    Eq,
    PartialEq,
    RlpEncodable,
    RlpDecodable,
    Serialize,
    Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    /// Nonce.
    pub nonce: U256,
    /// Gas price.
    pub gas_price: U256,
    /// Gas paid up front for transaction execution.
    pub gas: U256,
    /// Action, can be either call or contract create.
    pub action: Action,
    /// Transferred value.
    pub value: U256,
    /// Maximum storage increasement in this execution.
    pub storage_limit: u64,
    /// The epoch height of the transaction. A transaction
    /// can only be packed between the epochs of [epoch_height -
    /// TRANSACTION_EPOCH_BOUND, epoch_height + TRANSACTION_EPOCH_BOUND]
    pub epoch_height: u64,
    /// The chain id of the transaction
    pub chain_id: u32,
    /// Transaction data.
    pub data: Bytes,
}

impl Transaction {
    pub fn hash(&self) -> H256 {
        let mut s = RlpStream::new();
        s.append(self);
        keccak(s.as_raw())
    }

    pub fn sign(self, secret: &Secret) -> SignedTransaction {
        let sig = ::keylib::sign(secret, &self.hash())
            .expect("data is valid and context has signing capabilities; qed");
        let tx_with_sig = self.with_signature(sig);
        let public = tx_with_sig
            .recover_public()
            .expect("secret is valid so it's recoverable");
        SignedTransaction::new(public, tx_with_sig)
    }

    /// Specify the sender; this won't survive the serialize/deserialize
    /// process, but can be cloned.
    pub fn fake_sign(self, from: Address) -> SignedTransaction {
        SignedTransaction {
            transaction: TransactionWithSignature {
                transaction: TransactionWithSignatureSerializePart {
                    unsigned: self,
                    r: U256::one(),
                    s: U256::one(),
                    v: 0,
                },
                hash: H256::zero(),
                rlp_size: None,
            }
            .compute_hash(),
            sender: from,
            public: None,
        }
    }

    /// Signs the transaction with signature.
    pub fn with_signature(self, sig: Signature) -> TransactionWithSignature {
        TransactionWithSignature {
            transaction: TransactionWithSignatureSerializePart {
                unsigned: self,
                r: sig.r().into(),
                s: sig.s().into(),
                v: sig.v(),
            },
            hash: H256::zero(),
            rlp_size: None,
        }
        .compute_hash()
    }
}

impl MallocSizeOf for Transaction {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.data.size_of(ops)
    }
}

/// Signed transaction information without verified signature.
#[derive(
    Debug,
    Clone,
    Eq,
    PartialEq,
    RlpEncodable,
    RlpDecodable,
    Serialize,
    Deserialize,
)]
pub struct TransactionWithSignatureSerializePart {
    /// Plain Transaction.
    pub unsigned: Transaction,
    /// The V field of the signature; helps describe which half of the curve
    /// our point falls in.
    pub v: u8,
    /// The R field of the signature; helps describe the point on the curve.
    pub r: U256,
    /// The S field of the signature; helps describe the point on the curve.
    pub s: U256,
}

impl Deref for TransactionWithSignatureSerializePart {
    type Target = Transaction;

    fn deref(&self) -> &Self::Target { &self.unsigned }
}

/// Signed transaction information without verified signature.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionWithSignature {
    /// Serialize part.
    pub transaction: TransactionWithSignatureSerializePart,
    /// Hash of the transaction
    #[serde(skip)]
    pub hash: H256,
    /// The transaction size when serialized in rlp
    #[serde(skip)]
    pub rlp_size: Option<usize>,
}

impl Deref for TransactionWithSignature {
    type Target = TransactionWithSignatureSerializePart;

    fn deref(&self) -> &Self::Target { &self.transaction }
}

impl Decodable for TransactionWithSignature {
    fn decode(d: &Rlp) -> Result<Self, DecoderError> {
        let hash = keccak(d.as_raw());
        let rlp_size = Some(d.as_raw().len());
        // Check item count of TransactionWithSignatureSerializePart
        if d.item_count()? != 4 {
            return Err(DecoderError::RlpIncorrectListLen);
        }
        let transaction = d.as_val()?;
        Ok(TransactionWithSignature {
            transaction,
            hash,
            rlp_size,
        })
    }
}

impl Encodable for TransactionWithSignature {
    fn rlp_append(&self, s: &mut RlpStream) {
        s.append_internal(&self.transaction);
    }
}

impl TransactionWithSignature {
    pub fn new_unsigned(tx: Transaction) -> Self {
        TransactionWithSignature {
            transaction: TransactionWithSignatureSerializePart {
                unsigned: tx,
                s: 0.into(),
                r: 0.into(),
                v: 0,
            },
            hash: Default::default(),
            rlp_size: None,
        }
    }

    /// Used to compute hash of created transactions
    fn compute_hash(mut self) -> TransactionWithSignature {
        let hash = keccak(&*self.rlp_bytes());
        self.hash = hash;
        self
    }

    /// Checks whether signature is empty.
    pub fn is_unsigned(&self) -> bool { self.r.is_zero() && self.s.is_zero() }

    /// Construct a signature object from the sig.
    pub fn signature(&self) -> Signature {
        let r: H256 = BigEndianHash::from_uint(&self.r);
        let s: H256 = BigEndianHash::from_uint(&self.s);
        Signature::from_rsv(&r, &s, self.v)
    }

    /// Checks whether the signature has a low 's' value.
    pub fn check_low_s(&self) -> Result<(), keylib::Error> {
        if !self.signature().is_low_s() {
            Err(keylib::Error::InvalidSignature)
        } else {
            Ok(())
        }
    }

    pub fn hash(&self) -> H256 { self.hash }

    /// Recovers the public key of the sender.
    pub fn recover_public(&self) -> Result<Public, keylib::Error> {
        Ok(recover(&self.signature(), &self.unsigned.hash())?)
    }

    pub fn rlp_size(&self) -> usize {
        self.rlp_size.unwrap_or_else(|| self.rlp_bytes().len())
    }
}

impl MallocSizeOf for TransactionWithSignature {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.unsigned.size_of(ops)
    }
}

/// A signed transaction with successfully recovered `sender`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedTransaction {
    pub transaction: TransactionWithSignature,
    pub sender: Address,
    pub public: Option<Public>,
}

impl Encodable for SignedTransaction {
    fn rlp_append(&self, s: &mut RlpStream) {
        s.begin_list(3);
        s.append(&self.transaction);
        s.append(&self.sender);
        s.append(&self.public);
    }
}

impl Decodable for SignedTransaction {
    fn decode(rlp: &Rlp) -> Result<Self, DecoderError> {
        Ok(SignedTransaction {
            transaction: rlp.val_at(0)?,
            sender: rlp.val_at(1)?,
            public: rlp.val_at(2)?,
        })
    }
}

impl Deref for SignedTransaction {
    type Target = TransactionWithSignature;

    fn deref(&self) -> &Self::Target { &self.transaction }
}

impl From<SignedTransaction> for TransactionWithSignature {
    fn from(tx: SignedTransaction) -> Self { tx.transaction }
}

impl SignedTransaction {
    /// Try to verify transaction and recover sender.
    pub fn new(public: Public, transaction: TransactionWithSignature) -> Self {
        if transaction.is_unsigned() {
            SignedTransaction {
                transaction,
                sender: UNSIGNED_SENDER,
                public: None,
            }
        } else {
            let sender = public_to_address(&public);
            SignedTransaction {
                transaction,
                sender,
                public: Some(public),
            }
        }
    }

    pub fn new_unsigned(transaction: TransactionWithSignature) -> Self {
        SignedTransaction {
            transaction,
            sender: UNSIGNED_SENDER,
            public: None,
        }
    }

    pub fn set_public(&mut self, public: Public) {
        self.sender = public_to_address(&public);
        self.public = Some(public);
    }

    /// Returns transaction sender.
    pub fn sender(&self) -> Address { self.sender }

    pub fn nonce(&self) -> U256 { self.transaction.nonce }

    /// Checks if signature is empty.
    pub fn is_unsigned(&self) -> bool { self.transaction.is_unsigned() }

    pub fn hash(&self) -> H256 { self.transaction.hash() }

    pub fn gas(&self) -> &U256 { &self.transaction.gas }

    pub fn gas_price(&self) -> &U256 { &self.transaction.gas_price }

    pub fn gas_limit(&self) -> &U256 { &self.transaction.gas }

    pub fn rlp_size(&self) -> usize { self.transaction.rlp_size() }

    pub fn public(&self) -> &Option<Public> { &self.public }

    pub fn verify_public(&self, skip: bool) -> Result<bool, keylib::Error> {
        if self.public.is_none() {
            return Ok(false);
        }

        if !skip {
            let public = self.public.unwrap();
            Ok(verify_public(
                &public,
                &self.signature(),
                &self.unsigned.hash(),
            )?)
        } else {
            Ok(true)
        }
    }
}

impl MallocSizeOf for SignedTransaction {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.transaction.size_of(ops)
    }
}
#[cfg(test)]
mod tests {
    use crate::{
        transaction::UNSIGNED_SENDER, Action, ChainIdParams, SignedTransaction,
        Transaction, TransactionWithSignature,
        TransactionWithSignatureSerializePart,
    };
    use cfx_types::{Address, H160, H256, H512, U256};
    use core::str::FromStr;
    use keylib::{public_to_address, Signature};
    #[test]
    fn test_action() {
        let action_create = Action::Create;
        assert_eq!(Action::default(), action_create);
    }
    #[test]
    fn test_chain_id_params() {
        let chain_id = ChainIdParams { chain_id: 0 };
        assert_eq!(chain_id.get_chain_id(1), chain_id.chain_id)
    }
    #[test]
    fn test_transaction() {
        let transaction = Transaction {
            nonce: Default::default(),
            gas_price: Default::default(),
            gas: Default::default(),
            action: Default::default(),
            value: Default::default(),
            storage_limit: 0,
            epoch_height: 0,
            chain_id: 0,
            data: vec![],
        };

        let address = "0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6"
            .parse::<Address>()
            .unwrap();

        assert_eq!(transaction.hash(),H256::from_str("c5b2c658f5fa236c598a6e7fbf7f21413dc42e2a41dd982eb772b30707cba2eb").unwrap());
        let signed_trans = SignedTransaction {
            transaction: TransactionWithSignature {
                transaction: TransactionWithSignatureSerializePart {
                    unsigned: transaction.clone(),
                    r: U256::one(),
                    s: U256::one(),
                    v: 0,
                },
                hash: H256::zero(),
                rlp_size: None,
            }
            .compute_hash(),
            sender: address.clone(),
            public: None,
        };
        assert_eq!(
            transaction.clone().fake_sign(address.clone()),
            signed_trans.clone()
        );
        let sig = Signature::from([0; 65]);
        let trans_with_sig = TransactionWithSignature {
            transaction: TransactionWithSignatureSerializePart {
                unsigned: transaction.clone(),
                r: U256::zero(),
                s: U256::zero(),
                v: 0,
            },
            hash: H256::from_str("6afedf2d3f8fe6e19c0e9318a9af5c2034b0987f9990b1012e314286dcb51655").unwrap(),
            rlp_size: None,
        };
        assert_eq!(transaction.with_signature(sig), trans_with_sig);
    }
    #[test]
    fn test_transaction_with_sig() {
        let transaction = Transaction {
            nonce: Default::default(),
            gas_price: Default::default(),
            gas: Default::default(),
            action: Default::default(),
            value: Default::default(),
            storage_limit: 0,
            epoch_height: 0,
            chain_id: 0,
            data: vec![],
        };
        let trans_with_sig = TransactionWithSignature {
            transaction: TransactionWithSignatureSerializePart {
                unsigned: transaction.clone(),
                r: U256::zero(),
                s: U256::zero(),
                v: 0,
            },
            hash: H256::zero(),
            rlp_size: None,
        };
        let trans_with_sig_mut = TransactionWithSignature {
            transaction: TransactionWithSignatureSerializePart {
                unsigned: transaction.clone(),
                r: U256::zero(),
                s: U256::zero(),
                v: 0,
            },
            hash: H256::zero(),
            rlp_size: None,
        };
        assert_eq!(
            TransactionWithSignature::new_unsigned(transaction.clone()),
            trans_with_sig.clone()
        );
        assert_eq!(trans_with_sig.hash(), trans_with_sig.hash);
        assert_eq!(trans_with_sig.rlp_size(), 14);
        assert_eq!(trans_with_sig.is_unsigned(), true);
        let res = TransactionWithSignature{
            transaction: TransactionWithSignatureSerializePart {
                unsigned: Default::default(),
                v: 0,
                r: Default::default(),
                s: Default::default()
            },
            hash: H256::from_str("6afedf2d3f8fe6e19c0e9318a9af5c2034b0987f9990b1012e314286dcb51655").unwrap(),
            rlp_size: None
        };
        assert_eq!(trans_with_sig_mut.compute_hash(), res.clone());
        let sig = Signature::from([0; 65]);
        assert_eq!(trans_with_sig.signature(), sig);
        assert_eq!(trans_with_sig.check_low_s().is_ok(), true);
    }
    #[test]
    fn test_signed_trans() {
        let unsigned_trans = SignedTransaction {
            transaction: TransactionWithSignature {
                transaction: TransactionWithSignatureSerializePart {
                    unsigned: Default::default(),
                    v: 0,
                    r: Default::default(),
                    s: Default::default(),
                },
                hash: Default::default(),
                rlp_size: None,
            },
            sender: UNSIGNED_SENDER,
            public: None,
        };
        let sender = public_to_address(&H512::zero());
        let signed_trans = SignedTransaction {
            transaction: TransactionWithSignature {
                transaction: TransactionWithSignatureSerializePart {
                    unsigned: Default::default(),
                    v: 0,
                    r: U256::one(),
                    s: U256::one(),
                },
                hash: Default::default(),
                rlp_size: None,
            },
            sender,
            public: Some(H512::zero()),
        };
        let trans_with_sig = TransactionWithSignature {
            transaction: TransactionWithSignatureSerializePart {
                unsigned: Default::default(),
                v: 0,
                r: Default::default(),
                s: Default::default(),
            },
            hash: Default::default(),
            rlp_size: None,
        };
        let trans_with_sig1 = TransactionWithSignature {
            transaction: TransactionWithSignatureSerializePart {
                unsigned: Default::default(),
                v: 0,
                r: U256::one(),
                s: U256::one(),
            },
            hash: Default::default(),
            rlp_size: None,
        };
        assert_eq!(
            SignedTransaction::new(H512::zero(), trans_with_sig.clone()),
            unsigned_trans
        );
        assert_eq!(
            SignedTransaction::new(H512::zero(), trans_with_sig1.clone()),
            signed_trans
        );
        assert_eq!(
            SignedTransaction::new_unsigned(trans_with_sig.clone()),
            unsigned_trans
        );
        assert_eq!(unsigned_trans.sender(), H160([0xff; 20]));
        assert_eq!(unsigned_trans.nonce(), U256::zero());
        assert_eq!(unsigned_trans.hash(), H256::zero());
        assert_eq!(unsigned_trans.gas(), &U256::zero());
        assert_eq!(unsigned_trans.gas_price(), &U256::zero());
        assert_eq!(unsigned_trans.gas_limit(), &U256::zero());
        assert_eq!(unsigned_trans.rlp_size(), 14);
        assert_eq!(unsigned_trans.public(), &Option::None);
        assert_eq!(signed_trans.public(), &Option::Some(H512::zero()));
        let mut unsigned_trans_copy = unsigned_trans.clone();
        unsigned_trans_copy.set_public(H512::zero());
        assert_eq!(unsigned_trans_copy.public(), &Option::Some(H512::zero()));
        assert_eq!(signed_trans.is_unsigned(), false);
        assert_eq!(unsigned_trans.is_unsigned(), true);
        assert_eq!(signed_trans.verify_public(true).unwrap(), true);
        assert_eq!(signed_trans.verify_public(false).is_err(), true);
        assert_eq!(unsigned_trans_copy.verify_public(false).is_err(), true);
        let unsigned_trans1 = SignedTransaction {
            transaction: TransactionWithSignature {
                transaction: TransactionWithSignatureSerializePart {
                    unsigned: Default::default(),
                    v: 0,
                    r: Default::default(),
                    s: Default::default(),
                },
                hash: Default::default(),
                rlp_size: None,
            },
            sender: UNSIGNED_SENDER,
            public: None,
        };
        assert_eq!(unsigned_trans1.verify_public(false).unwrap(), false);
    }
}
