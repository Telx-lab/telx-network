use crate::execution ::{Address, H256};
use execution_codecs::{main_codec, Compact};
use execution_rlp::{RlpDecodable, RlpDecodableWrapper, RlpEncodable, RlpEncodableWrapper};
use revm_primitives::U256;
use serde::{Deserialize, Serialize};

/// A list of addresses and storage keys that the transaction plans to access.
/// Accesses outside the list are possible, but become more expensive.
#[main_codec(rlp)]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default, RlpDecodable, RlpEncodable)]
#[serde(rename_all = "camelCase")]
pub struct AccessListItem {
    /// Account addresses that would be loaded at the start of execution
    pub address: Address,
    /// Keys of storage that would be loaded at the start of execution
    #[cfg_attr(
        any(test, feature = "arbitrary"),
        proptest(
            strategy = "proptest::collection::vec(proptest::arbitrary::any::<H256>(), 0..=20)"
        )
    )]
    pub storage_keys: Vec<H256>,
}

/// AccessList as defined in EIP-2930
#[main_codec(rlp)]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default, RlpDecodableWrapper, RlpEncodableWrapper)]
pub struct AccessList(
    #[cfg_attr(
        any(test, feature = "arbitrary"),
        proptest(
            strategy = "proptest::collection::vec(proptest::arbitrary::any::<AccessListItem>(), 0..=20)"
        )
    )]
    pub Vec<AccessListItem>,
);

impl AccessList {
    /// Converts the list into a vec, expected by revm
    pub fn flattened(self) -> Vec<(Address, Vec<U256>)> {
        self.0
            .into_iter()
            .map(|item| {
                (
                    item.address,
                    item.storage_keys.into_iter().map(|slot| U256::from_be_bytes(slot.0)).collect(),
                )
            })
            .collect()
    }
}

/// Access list with gas used appended.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AccessListWithGasUsed {
    /// List with accounts accessed during transaction.
    pub access_list: AccessList,
    /// Estimated gas used with access list.
    pub gas_used: U256,
}
