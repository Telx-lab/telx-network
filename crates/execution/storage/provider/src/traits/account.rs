use auto_impl::auto_impl;
use execution_interfaces::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::{RangeBounds, RangeInclusive},
};
use tn_types::execution::{Account, Address, BlockNumber};

/// Account reader
#[auto_impl(&, Arc, Box)]
pub trait AccountReader: Send + Sync {
    /// Get basic account information.
    ///
    /// Returns `None` if the account doesn't exist.
    fn basic_account(&self, address: Address) -> Result<Option<Account>>;
}

/// Account reader
#[auto_impl(&, Arc, Box)]
pub trait AccountExtReader: Send + Sync {
    /// Iterate over account changesets and return all account address that were changed.
    fn changed_accounts_with_range(
        &self,
        _range: impl RangeBounds<BlockNumber>,
    ) -> Result<BTreeSet<Address>>;

    /// Get basic account information for multiple accounts. A more efficient version than calling
    /// [`AccountReader::basic_account`] repeatedly.
    ///
    /// Returns `None` if the account doesn't exist.
    fn basic_accounts(
        &self,
        _iter: impl IntoIterator<Item = Address>,
    ) -> Result<Vec<(Address, Option<Account>)>>;

    /// Iterate over account changesets and return all account addresses that were changed alongside
    /// each specific set of blocks.
    ///
    /// NOTE: Get inclusive range of blocks.
    fn changed_accounts_and_blocks_with_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> Result<BTreeMap<Address, Vec<BlockNumber>>>;
}
