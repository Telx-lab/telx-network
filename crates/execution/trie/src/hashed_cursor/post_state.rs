use super::{HashedAccountCursor, HashedCursorFactory, HashedStorageCursor};
use crate::prefix_set::{PrefixSet, PrefixSetMut};
use execution_db::{
    cursor::{DbCursorRO, DbDupCursorRO},
    tables,
    transaction::{DbTx, DbTxGAT},
};
use std::collections::{HashMap, HashSet};
use tn_types::execution::{trie::Nibbles, Account, StorageEntry, H256, U256};

/// The post state account storage with hashed slots.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HashedStorage {
    /// Hashed storage slots with non-zero.
    non_zero_valued_storage: Vec<(H256, U256)>,
    /// Slots that have been zero valued.
    zero_valued_slots: HashSet<H256>,
    /// Whether the storage was wiped or not.
    wiped: bool,
    /// Whether the storage entries were sorted or not.
    sorted: bool,
}

impl HashedStorage {
    /// Create new instance of [HashedStorage].
    pub fn new(wiped: bool) -> Self {
        Self {
            non_zero_valued_storage: Vec::new(),
            zero_valued_slots: HashSet::new(),
            wiped,
            sorted: true, // empty is sorted
        }
    }

    /// Sorts the non zero value storage entries.
    pub fn sort_storage(&mut self) {
        if !self.sorted {
            self.non_zero_valued_storage.sort_unstable_by_key(|(slot, _)| *slot);
            self.sorted = true;
        }
    }

    /// Insert non zero-valued storage entry.
    pub fn insert_non_zero_valued_storage(&mut self, slot: H256, value: U256) {
        debug_assert!(value != U256::ZERO, "value cannot be zero");
        self.non_zero_valued_storage.push((slot, value));
        self.sorted = false;
    }

    /// Insert zero-valued storage slot.
    pub fn insert_zero_valued_slot(&mut self, slot: H256) {
        self.zero_valued_slots.insert(slot);
    }
}

/// The post state with hashed addresses as keys.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HashedPostState {
    /// Map of hashed addresses to account info.
    accounts: Vec<(H256, Account)>,
    /// Set of cleared accounts.
    cleared_accounts: HashSet<H256>,
    /// Map of hashed addresses to hashed storage.
    storages: HashMap<H256, HashedStorage>,
    /// Whether the account and storage entries were sorted or not.
    sorted: bool,
}

impl Default for HashedPostState {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            cleared_accounts: HashSet::new(),
            storages: HashMap::new(),
            sorted: true, // empty is sorted
        }
    }
}

impl HashedPostState {
    /// Sort and return self.
    pub fn sorted(mut self) -> Self {
        self.sort();
        self
    }

    /// Sort account and storage entries.
    pub fn sort(&mut self) {
        if !self.sorted {
            for (_, storage) in self.storages.iter_mut() {
                storage.sort_storage();
            }

            self.accounts.sort_unstable_by_key(|(address, _)| *address);
            self.sorted = true;
        }
    }

    /// Insert non-empty account info.
    pub fn insert_account(&mut self, hashed_address: H256, account: Account) {
        self.accounts.push((hashed_address, account));
        self.sorted = false;
    }

    /// Insert cleared hashed account key.
    pub fn insert_cleared_account(&mut self, hashed_address: H256) {
        self.cleared_accounts.insert(hashed_address);
    }

    /// Insert hashed storage entry.
    pub fn insert_hashed_storage(&mut self, hashed_address: H256, hashed_storage: HashedStorage) {
        self.sorted &= hashed_storage.sorted;
        self.storages.insert(hashed_address, hashed_storage);
    }

    /// Construct (PrefixSet)[PrefixSet] from hashed post state.
    /// The prefix sets contain the hashed account and storage keys that have been changed in the
    /// post state.
    pub fn construct_prefix_sets(&self) -> (PrefixSet, HashMap<H256, PrefixSet>) {
        // Initialize prefix sets.
        let mut account_prefix_set = PrefixSetMut::default();
        let mut storage_prefix_set: HashMap<H256, PrefixSetMut> = HashMap::default();

        // Populate account prefix set.
        for (hashed_address, _) in &self.accounts {
            account_prefix_set.insert(Nibbles::unpack(hashed_address));
        }
        for hashed_address in &self.cleared_accounts {
            account_prefix_set.insert(Nibbles::unpack(hashed_address));
        }

        // Populate storage prefix sets.
        for (hashed_address, hashed_storage) in self.storages.iter() {
            account_prefix_set.insert(Nibbles::unpack(hashed_address));

            let storage_prefix_set_entry = storage_prefix_set.entry(*hashed_address).or_default();
            for (hashed_slot, _) in &hashed_storage.non_zero_valued_storage {
                storage_prefix_set_entry.insert(Nibbles::unpack(hashed_slot));
            }
            for hashed_slot in &hashed_storage.zero_valued_slots {
                storage_prefix_set_entry.insert(Nibbles::unpack(hashed_slot));
            }
        }

        (
            account_prefix_set.freeze(),
            storage_prefix_set.into_iter().map(|(k, v)| (k, v.freeze())).collect(),
        )
    }
}

/// The hashed cursor factory for the post state.
pub struct HashedPostStateCursorFactory<'a, 'b, TX> {
    tx: &'a TX,
    post_state: &'b HashedPostState,
}

impl<'a, 'b, TX> HashedPostStateCursorFactory<'a, 'b, TX> {
    /// Create a new factory.
    pub fn new(tx: &'a TX, post_state: &'b HashedPostState) -> Self {
        Self { tx, post_state }
    }
}

impl<'a, 'b, 'tx, TX: DbTx<'tx>> HashedCursorFactory<'a>
    for HashedPostStateCursorFactory<'a, 'b, TX>
where
    'a: 'b,
{
    type AccountCursor = HashedPostStateAccountCursor<'b, <TX as DbTxGAT<'a>>::Cursor<tables::HashedAccount>> where Self: 'a;
    type StorageCursor = HashedPostStateStorageCursor<'b, <TX as DbTxGAT<'a>>::DupCursor<tables::HashedStorage>> where Self: 'a;

    fn hashed_account_cursor(&'a self) -> Result<Self::AccountCursor, execution_db::DatabaseError> {
        let cursor = self.tx.cursor_read::<tables::HashedAccount>()?;
        Ok(HashedPostStateAccountCursor::new(cursor, self.post_state))
    }

    fn hashed_storage_cursor(&'a self) -> Result<Self::StorageCursor, execution_db::DatabaseError> {
        let cursor = self.tx.cursor_dup_read::<tables::HashedStorage>()?;
        Ok(HashedPostStateStorageCursor::new(cursor, self.post_state))
    }
}

/// The cursor to iterate over post state hashed accounts and corresponding database entries.
/// It will always give precedence to the data from the hashed post state.
#[derive(Debug, Clone)]
pub struct HashedPostStateAccountCursor<'b, C> {
    /// The database cursor.
    cursor: C,
    /// The reference to the in-memory [HashedPostState].
    post_state: &'b HashedPostState,
    /// The post state account index where the cursor is currently at.
    post_state_account_index: usize,
    /// The last hashed account key that was returned by the cursor.
    /// De facto, this is a current cursor position.
    last_account: Option<H256>,
}

impl<'b, C> HashedPostStateAccountCursor<'b, C> {
    /// Create new instance of [HashedPostStateAccountCursor].
    pub fn new(cursor: C, post_state: &'b HashedPostState) -> Self {
        Self { cursor, post_state, last_account: None, post_state_account_index: 0 }
    }

    /// Returns `true` if the account has been destroyed.
    /// This check is used for evicting account keys from the state trie.
    ///
    /// This function only checks the post state, not the database, because the latter does not
    /// store destroyed accounts.
    fn is_account_cleared(&self, account: &H256) -> bool {
        self.post_state.cleared_accounts.contains(account)
    }

    /// Return the account with the lowest hashed account key.
    ///
    /// Given the next post state and database entries, return the smallest of the two.
    /// If the account keys are the same, the post state entry is given precedence.
    fn next_account(
        post_state_item: Option<&(H256, Account)>,
        db_item: Option<(H256, Account)>,
    ) -> Option<(H256, Account)> {
        match (post_state_item, db_item) {
            // If both are not empty, return the smallest of the two
            // Post state is given precedence if keys are equal
            (Some((post_state_address, post_state_account)), Some((db_address, db_account))) => {
                if post_state_address <= &db_address {
                    Some((*post_state_address, *post_state_account))
                } else {
                    Some((db_address, db_account))
                }
            }
            // If the database is empty, return the post state entry
            (Some((post_state_address, post_state_account)), None) => {
                Some((*post_state_address, *post_state_account))
            }
            // If the post state is empty, return the database entry
            (None, Some((db_address, db_account))) => Some((db_address, db_account)),
            // If both are empty, return None
            (None, None) => None,
        }
    }
}

impl<'b, 'tx, C> HashedAccountCursor for HashedPostStateAccountCursor<'b, C>
where
    C: DbCursorRO<'tx, tables::HashedAccount>,
{
    /// Seek the next entry for a given hashed account key.
    ///
    /// If the post state contains the exact match for the key, return it.
    /// Otherwise, retrieve the next entries that are greater than or equal to the key from the
    /// database and the post state. The two entries are compared and the lowest is returned.
    ///
    /// The returned account key is memoized and the cursor remains positioned at that key until
    /// [HashedAccountCursor::seek] or [HashedAccountCursor::next] are called.
    fn seek(&mut self, key: H256) -> Result<Option<(H256, Account)>, execution_db::DatabaseError> {
        debug_assert!(self.post_state.sorted, "`HashedPostState` must be pre-sorted");

        self.last_account = None;

        // Take the next account from the post state with the key greater than or equal to the
        // sought key.
        let mut post_state_entry = self.post_state.accounts.get(self.post_state_account_index);
        while let Some((k, _)) = post_state_entry {
            if k >= &key {
                // Found the next entry that is equal or greater than the key.
                break
            }

            self.post_state_account_index += 1;
            post_state_entry = self.post_state.accounts.get(self.post_state_account_index);
        }

        // It's an exact match, return the account from post state without looking up in the
        // database.
        if let Some((address, account)) = post_state_entry {
            if address == &key {
                self.last_account = Some(*address);
                return Ok(Some((*address, *account)))
            }
        }

        // It's not an exact match, reposition to the first greater or equal account that wasn't
        // cleared.
        let mut db_entry = self.cursor.seek(key)?;
        while db_entry
            .as_ref()
            .map(|(address, _)| self.is_account_cleared(address))
            .unwrap_or_default()
        {
            db_entry = self.cursor.next()?;
        }

        // Compare two entries and return the lowest.
        let result = Self::next_account(post_state_entry, db_entry);
        self.last_account = result.as_ref().map(|(address, _)| *address);
        Ok(result)
    }

    /// Retrieve the next entry from the cursor.
    ///
    /// If the cursor is positioned at the entry, return the entry with next greater key.
    /// Returns [None] if the previous memoized or the next greater entries are missing.
    ///
    /// NOTE: This function will not return any entry unless [HashedAccountCursor::seek] has been
    /// called.
    fn next(&mut self) -> Result<Option<(H256, Account)>, execution_db::DatabaseError> {
        debug_assert!(self.post_state.sorted, "`HashedPostState` must be pre-sorted");

        let last_account = match self.last_account.as_ref() {
            Some(account) => account,
            None => return Ok(None), // no previous entry was found
        };

        // If post state was given precedence, move the cursor forward.
        let mut db_entry = self.cursor.current()?;
        while db_entry
            .as_ref()
            .map(|(address, _)| address <= last_account || self.is_account_cleared(address))
            .unwrap_or_default()
        {
            db_entry = self.cursor.next()?;
        }

        // Take the next account from the post state with the key greater than or equal to the
        // sought key.
        let mut post_state_entry = self.post_state.accounts.get(self.post_state_account_index);
        while let Some((k, _)) = post_state_entry {
            if k > last_account {
                // Found the next entry in the post state.
                break
            }

            self.post_state_account_index += 1;
            post_state_entry = self.post_state.accounts.get(self.post_state_account_index);
        }

        // Compare two entries and return the lowest.
        let result = Self::next_account(post_state_entry, db_entry);
        self.last_account = result.as_ref().map(|(address, _)| *address);
        Ok(result)
    }
}

/// The cursor to iterate over post state hashed storages and corresponding database entries.
/// It will always give precedence to the data from the post state.
#[derive(Debug, Clone)]
pub struct HashedPostStateStorageCursor<'b, C> {
    /// The database cursor.
    cursor: C,
    /// The reference to the post state.
    post_state: &'b HashedPostState,
    /// The post state index where the cursor is currently at.
    post_state_storage_index: usize,
    /// The current hashed account key.
    account: Option<H256>,
    /// The last slot that has been returned by the cursor.
    /// De facto, this is the cursor's position for the given account key.
    last_slot: Option<H256>,
}

impl<'b, C> HashedPostStateStorageCursor<'b, C> {
    /// Create new instance of [HashedPostStateStorageCursor].
    pub fn new(cursor: C, post_state: &'b HashedPostState) -> Self {
        Self { cursor, post_state, account: None, last_slot: None, post_state_storage_index: 0 }
    }

    /// Returns `true` if the storage for the given
    /// The database is not checked since it already has no wiped storage entries.
    fn is_db_storage_wiped(&self, account: &H256) -> bool {
        match self.post_state.storages.get(account) {
            Some(storage) => storage.wiped,
            None => false,
        }
    }

    /// Check if the slot was zeroed out in the post state.
    /// The database is not checked since it already has no zero-valued slots.
    fn is_slot_zero_valued(&self, account: &H256, slot: &H256) -> bool {
        self.post_state
            .storages
            .get(account)
            .map(|storage| storage.zero_valued_slots.contains(slot))
            .unwrap_or_default()
    }

    /// Return the storage entry with the lowest hashed storage key (hashed slot).
    ///
    /// Given the next post state and database entries, return the smallest of the two.
    /// If the storage keys are the same, the post state entry is given precedence.
    fn next_slot(
        &self,
        post_state_item: Option<&(H256, U256)>,
        db_item: Option<StorageEntry>,
    ) -> Option<StorageEntry> {
        match (post_state_item, db_item) {
            // If both are not empty, return the smallest of the two
            // Post state is given precedence if keys are equal
            (Some((post_state_slot, post_state_value)), Some(db_entry)) => {
                if post_state_slot <= &db_entry.key {
                    Some(StorageEntry { key: *post_state_slot, value: *post_state_value })
                } else {
                    Some(db_entry)
                }
            }
            // If the database is empty, return the post state entry
            (Some((post_state_slot, post_state_value)), None) => {
                Some(StorageEntry { key: *post_state_slot, value: *post_state_value })
            }
            // If the post state is empty, return the database entry
            (None, Some(db_entry)) => Some(db_entry),
            // If both are empty, return None
            (None, None) => None,
        }
    }
}

impl<'b, 'tx, C> HashedStorageCursor for HashedPostStateStorageCursor<'b, C>
where
    C: DbCursorRO<'tx, tables::HashedStorage> + DbDupCursorRO<'tx, tables::HashedStorage>,
{
    /// Returns `true` if the account has no storage entries.
    ///
    /// This function should be called before attempting to call [HashedStorageCursor::seek] or
    /// [HashedStorageCursor::next].
    fn is_storage_empty(&mut self, key: H256) -> Result<bool, execution_db::DatabaseError> {
        let is_empty = match self.post_state.storages.get(&key) {
            Some(storage) => {
                // If the storage has been wiped at any point
                storage.wiped &&
                    // and the current storage does not contain any non-zero values 
                    storage.non_zero_valued_storage.is_empty()
            }
            None => self.cursor.seek_exact(key)?.is_none(),
        };
        Ok(is_empty)
    }

    /// Seek the next account storage entry for a given hashed key pair.
    fn seek(
        &mut self,
        account: H256,
        subkey: H256,
    ) -> Result<Option<StorageEntry>, execution_db::DatabaseError> {
        if self.account.map_or(true, |acc| acc != account) {
            self.account = Some(account);
            self.last_slot = None;
            self.post_state_storage_index = 0;
        }

        // Attempt to find the account's storage in post state.
        let mut post_state_entry = None;
        if let Some(storage) = self.post_state.storages.get(&account) {
            debug_assert!(storage.sorted, "`HashStorage` must be pre-sorted");

            post_state_entry = storage.non_zero_valued_storage.get(self.post_state_storage_index);

            while let Some((slot, _)) = post_state_entry {
                if slot >= &subkey {
                    // Found the next entry that is equal or greater than the key.
                    break
                }

                self.post_state_storage_index += 1;
                post_state_entry =
                    storage.non_zero_valued_storage.get(self.post_state_storage_index);
            }
        }

        // It's an exact match, return the storage slot from post state without looking up in
        // the database.
        if let Some((slot, value)) = post_state_entry {
            if slot == &subkey {
                self.last_slot = Some(*slot);
                return Ok(Some(StorageEntry { key: *slot, value: *value }))
            }
        }

        // It's not an exact match, reposition to the first greater or equal account.
        let db_entry = if self.is_db_storage_wiped(&account) {
            None
        } else {
            let mut db_entry = self.cursor.seek_by_key_subkey(account, subkey)?;

            while db_entry
                .as_ref()
                .map(|entry| self.is_slot_zero_valued(&account, &entry.key))
                .unwrap_or_default()
            {
                db_entry = self.cursor.next_dup_val()?;
            }

            db_entry
        };

        // Compare two entries and return the lowest.
        let result = self.next_slot(post_state_entry, db_entry);
        self.last_slot = result.as_ref().map(|entry| entry.key);
        Ok(result)
    }

    /// Return the next account storage entry for the current accont key.
    ///
    /// # Panics
    ///
    /// If the account key is not set. [HashedStorageCursor::seek] must be called first in order to
    /// position the cursor.
    fn next(&mut self) -> Result<Option<StorageEntry>, execution_db::DatabaseError> {
        let account = self.account.expect("`seek` must be called first");

        let last_slot = match self.last_slot.as_ref() {
            Some(account) => account,
            None => return Ok(None), // no previous entry was found
        };

        let db_entry = if self.is_db_storage_wiped(&account) {
            None
        } else {
            // If post state was given precedence, move the cursor forward.
            let mut db_entry = self.cursor.seek_by_key_subkey(account, *last_slot)?;

            // If the entry was already returned, move to the next.
            if db_entry.as_ref().map(|entry| &entry.key == last_slot).unwrap_or_default() {
                db_entry = self.cursor.next_dup_val()?;
            }

            while db_entry
                .as_ref()
                .map(|entry| self.is_slot_zero_valued(&account, &entry.key))
                .unwrap_or_default()
            {
                db_entry = self.cursor.next_dup_val()?;
            }

            db_entry
        };

        // Attempt to find the account's storage in post state.
        let mut post_state_entry = None;
        if let Some(storage) = self.post_state.storages.get(&account) {
            debug_assert!(storage.sorted, "`HashStorage` must be pre-sorted");

            post_state_entry = storage.non_zero_valued_storage.get(self.post_state_storage_index);

            while let Some((k, _)) = post_state_entry {
                if k > last_slot {
                    // Found the next entry.
                    break
                }

                self.post_state_storage_index += 1;
                post_state_entry =
                    storage.non_zero_valued_storage.get(self.post_state_storage_index);
            }
        }

        // Compare two entries and return the lowest.
        let result = self.next_slot(post_state_entry, db_entry);
        self.last_slot = result.as_ref().map(|entry| entry.key);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execution_db::{database::Database, test_utils::create_test_rw_db, transaction::DbTxMut};
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    fn assert_account_cursor_order<'a, 'b>(
        factory: &'a impl HashedCursorFactory<'b>,
        mut expected: impl Iterator<Item = (H256, Account)>,
    ) where
        'a: 'b,
    {
        let mut cursor = factory.hashed_account_cursor().unwrap();

        let first_account = cursor.seek(H256::default()).unwrap();
        assert_eq!(first_account, expected.next());

        for expected in expected {
            let next_cursor_account = cursor.next().unwrap();
            assert_eq!(next_cursor_account, Some(expected));
        }

        assert!(cursor.next().unwrap().is_none());
    }

    fn assert_storage_cursor_order<'a, 'b>(
        factory: &'a impl HashedCursorFactory<'b>,
        expected: impl Iterator<Item = (H256, BTreeMap<H256, U256>)>,
    ) where
        'a: 'b,
    {
        let mut cursor = factory.hashed_storage_cursor().unwrap();

        for (account, storage) in expected {
            let mut expected_storage = storage.into_iter();

            let first_storage = cursor.seek(account, H256::default()).unwrap();
            assert_eq!(first_storage.map(|e| (e.key, e.value)), expected_storage.next());

            for expected_entry in expected_storage {
                let next_cursor_storage = cursor.next().unwrap();
                assert_eq!(next_cursor_storage.map(|e| (e.key, e.value)), Some(expected_entry));
            }

            assert!(cursor.next().unwrap().is_none());
        }
    }

    #[test]
    fn post_state_only_accounts() {
        let accounts =
            Vec::from_iter((1..11).map(|key| (H256::from_low_u64_be(key), Account::default())));

        let mut hashed_post_state = HashedPostState::default();
        for (hashed_address, account) in &accounts {
            hashed_post_state.insert_account(*hashed_address, *account);
        }
        hashed_post_state.sort();

        let db = create_test_rw_db();
        let tx = db.tx().unwrap();

        let factory = HashedPostStateCursorFactory::new(&tx, &hashed_post_state);
        assert_account_cursor_order(&factory, accounts.into_iter());
    }

    #[test]
    fn db_only_accounts() {
        let accounts =
            Vec::from_iter((1..11).map(|key| (H256::from_low_u64_be(key), Account::default())));

        let db = create_test_rw_db();
        db.update(|tx| {
            for (key, account) in accounts.iter() {
                tx.put::<tables::HashedAccount>(*key, *account).unwrap();
            }
        })
        .unwrap();

        let tx = db.tx().unwrap();
        let post_state = HashedPostState::default();
        let factory = HashedPostStateCursorFactory::new(&tx, &post_state);
        assert_account_cursor_order(&factory, accounts.into_iter());
    }

    #[test]
    fn account_cursor_correct_order() {
        // odd keys are in post state, even keys are in db
        let accounts =
            Vec::from_iter((1..111).map(|key| (H256::from_low_u64_be(key), Account::default())));

        let db = create_test_rw_db();
        db.update(|tx| {
            for (key, account) in accounts.iter().filter(|x| x.0.to_low_u64_be() % 2 == 0) {
                tx.put::<tables::HashedAccount>(*key, *account).unwrap();
            }
        })
        .unwrap();

        let mut hashed_post_state = HashedPostState::default();
        for (hashed_address, account) in accounts.iter().filter(|x| x.0.to_low_u64_be() % 2 != 0) {
            hashed_post_state.insert_account(*hashed_address, *account);
        }
        hashed_post_state.sort();

        let tx = db.tx().unwrap();
        let factory = HashedPostStateCursorFactory::new(&tx, &hashed_post_state);
        assert_account_cursor_order(&factory, accounts.into_iter());
    }

    #[test]
    fn removed_accounts_are_discarded() {
        // odd keys are in post state, even keys are in db
        let accounts =
            Vec::from_iter((1..111).map(|key| (H256::from_low_u64_be(key), Account::default())));
        // accounts 5, 9, 11 should be considered removed from post state
        let removed_keys = Vec::from_iter([5, 9, 11].into_iter().map(H256::from_low_u64_be));

        let db = create_test_rw_db();
        db.update(|tx| {
            for (key, account) in accounts.iter().filter(|x| x.0.to_low_u64_be() % 2 == 0) {
                tx.put::<tables::HashedAccount>(*key, *account).unwrap();
            }
        })
        .unwrap();

        let mut hashed_post_state = HashedPostState::default();
        for (hashed_address, account) in accounts.iter().filter(|x| x.0.to_low_u64_be() % 2 != 0) {
            if removed_keys.contains(hashed_address) {
                hashed_post_state.insert_cleared_account(*hashed_address);
            } else {
                hashed_post_state.insert_account(*hashed_address, *account);
            }
        }
        hashed_post_state.sort();

        let tx = db.tx().unwrap();
        let factory = HashedPostStateCursorFactory::new(&tx, &hashed_post_state);
        let expected = accounts.into_iter().filter(|x| !removed_keys.contains(&x.0));
        assert_account_cursor_order(&factory, expected);
    }

    #[test]
    fn post_state_accounts_take_precedence() {
        let accounts =
            Vec::from_iter((1..10).map(|key| {
                (H256::from_low_u64_be(key), Account { nonce: key, ..Default::default() })
            }));

        let db = create_test_rw_db();
        db.update(|tx| {
            for (key, _) in accounts.iter() {
                // insert zero value accounts to the database
                tx.put::<tables::HashedAccount>(*key, Account::default()).unwrap();
            }
        })
        .unwrap();

        let mut hashed_post_state = HashedPostState::default();
        for (hashed_address, account) in &accounts {
            hashed_post_state.insert_account(*hashed_address, *account);
        }
        hashed_post_state.sort();

        let tx = db.tx().unwrap();
        let factory = HashedPostStateCursorFactory::new(&tx, &hashed_post_state);
        assert_account_cursor_order(&factory, accounts.into_iter());
    }

    #[test]
    fn fuzz_hashed_account_cursor() {
        proptest!(ProptestConfig::with_cases(10), |(db_accounts: BTreeMap<H256, Account>, post_state_accounts: BTreeMap<H256, Option<Account>>)| {
                let db = create_test_rw_db();
                db.update(|tx| {
                    for (key, account) in db_accounts.iter() {
                        tx.put::<tables::HashedAccount>(*key, *account).unwrap();
                    }
                })
                .unwrap();

                let mut hashed_post_state = HashedPostState::default();
                for (hashed_address, account) in &post_state_accounts {
                    if let Some(account) = account {
                        hashed_post_state.insert_account(*hashed_address, *account);
                    } else {
                        hashed_post_state.insert_cleared_account(*hashed_address);
                    }
                }
                hashed_post_state.sort();

                let mut expected = db_accounts;
                // overwrite or remove accounts from the expected result
                for (key, account) in post_state_accounts.iter() {
                    if let Some(account) = account {
                        expected.insert(*key, *account);
                    } else {
                        expected.remove(key);
                    }
                }

                let tx = db.tx().unwrap();
                let factory = HashedPostStateCursorFactory::new(&tx, &hashed_post_state);
                assert_account_cursor_order(&factory, expected.into_iter());
            }
        );
    }

    #[test]
    fn storage_is_empty() {
        let address = H256::random();
        let db = create_test_rw_db();

        // empty from the get go
        {
            let post_state = HashedPostState::default();
            let tx = db.tx().unwrap();
            let factory = HashedPostStateCursorFactory::new(&tx, &post_state);
            let mut cursor = factory.hashed_storage_cursor().unwrap();
            assert!(cursor.is_storage_empty(address).unwrap());
        }

        let db_storage =
            BTreeMap::from_iter((0..10).map(|key| (H256::from_low_u64_be(key), U256::from(key))));
        db.update(|tx| {
            for (slot, value) in db_storage.iter() {
                // insert zero value accounts to the database
                tx.put::<tables::HashedStorage>(
                    address,
                    StorageEntry { key: *slot, value: *value },
                )
                .unwrap();
            }
        })
        .unwrap();

        // not empty
        {
            let post_state = HashedPostState::default();
            let tx = db.tx().unwrap();
            let factory = HashedPostStateCursorFactory::new(&tx, &post_state);
            let mut cursor = factory.hashed_storage_cursor().unwrap();
            assert!(!cursor.is_storage_empty(address).unwrap());
        }

        // wiped storage, must be empty
        {
            let wiped = true;
            let hashed_storage = HashedStorage::new(wiped);

            let mut hashed_post_state = HashedPostState::default();
            hashed_post_state.insert_hashed_storage(address, hashed_storage);

            let tx = db.tx().unwrap();
            let factory = HashedPostStateCursorFactory::new(&tx, &hashed_post_state);
            let mut cursor = factory.hashed_storage_cursor().unwrap();
            assert!(cursor.is_storage_empty(address).unwrap());
        }

        // wiped storage, but post state has zero-value entries
        {
            let wiped = true;
            let mut hashed_storage = HashedStorage::new(wiped);
            hashed_storage.insert_zero_valued_slot(H256::random());

            let mut hashed_post_state = HashedPostState::default();
            hashed_post_state.insert_hashed_storage(address, hashed_storage);

            let tx = db.tx().unwrap();
            let factory = HashedPostStateCursorFactory::new(&tx, &hashed_post_state);
            let mut cursor = factory.hashed_storage_cursor().unwrap();
            assert!(cursor.is_storage_empty(address).unwrap());
        }

        // wiped storage, but post state has non-zero entries
        {
            let wiped = true;
            let mut hashed_storage = HashedStorage::new(wiped);
            hashed_storage.insert_non_zero_valued_storage(H256::random(), U256::from(1));

            let mut hashed_post_state = HashedPostState::default();
            hashed_post_state.insert_hashed_storage(address, hashed_storage);

            let tx = db.tx().unwrap();
            let factory = HashedPostStateCursorFactory::new(&tx, &hashed_post_state);
            let mut cursor = factory.hashed_storage_cursor().unwrap();
            assert!(!cursor.is_storage_empty(address).unwrap());
        }
    }

    #[test]
    fn storage_cursor_correct_order() {
        let address = H256::random();
        let db_storage =
            BTreeMap::from_iter((1..11).map(|key| (H256::from_low_u64_be(key), U256::from(key))));
        let post_state_storage =
            BTreeMap::from_iter((11..21).map(|key| (H256::from_low_u64_be(key), U256::from(key))));

        let db = create_test_rw_db();
        db.update(|tx| {
            for (slot, value) in db_storage.iter() {
                // insert zero value accounts to the database
                tx.put::<tables::HashedStorage>(
                    address,
                    StorageEntry { key: *slot, value: *value },
                )
                .unwrap();
            }
        })
        .unwrap();

        let wiped = false;
        let mut hashed_storage = HashedStorage::new(wiped);
        for (slot, value) in post_state_storage.iter() {
            hashed_storage.insert_non_zero_valued_storage(*slot, *value);
        }

        let mut hashed_post_state = HashedPostState::default();
        hashed_post_state.insert_hashed_storage(address, hashed_storage);
        hashed_post_state.sort();

        let tx = db.tx().unwrap();
        let factory = HashedPostStateCursorFactory::new(&tx, &hashed_post_state);
        let expected =
            [(address, db_storage.into_iter().chain(post_state_storage.into_iter()).collect())]
                .into_iter();
        assert_storage_cursor_order(&factory, expected);
    }

    #[test]
    fn zero_value_storage_entries_are_discarded() {
        let address = H256::random();
        let db_storage =
            BTreeMap::from_iter((0..10).map(|key| (H256::from_low_u64_be(key), U256::from(key)))); // every even number is changed to zero value
        let post_state_storage = BTreeMap::from_iter((0..10).map(|key| {
            (H256::from_low_u64_be(key), if key % 2 == 0 { U256::ZERO } else { U256::from(key) })
        }));

        let db = create_test_rw_db();
        db.update(|tx| {
            for (slot, value) in db_storage {
                // insert zero value accounts to the database
                tx.put::<tables::HashedStorage>(address, StorageEntry { key: slot, value })
                    .unwrap();
            }
        })
        .unwrap();

        let wiped = false;
        let mut hashed_storage = HashedStorage::new(wiped);
        for (slot, value) in post_state_storage.iter() {
            if *value == U256::ZERO {
                hashed_storage.insert_zero_valued_slot(*slot);
            } else {
                hashed_storage.insert_non_zero_valued_storage(*slot, *value);
            }
        }

        let mut hashed_post_state = HashedPostState::default();
        hashed_post_state.insert_hashed_storage(address, hashed_storage);
        hashed_post_state.sort();

        let tx = db.tx().unwrap();
        let factory = HashedPostStateCursorFactory::new(&tx, &hashed_post_state);
        let expected = [(
            address,
            post_state_storage.into_iter().filter(|(_, value)| *value > U256::ZERO).collect(),
        )]
        .into_iter();
        assert_storage_cursor_order(&factory, expected);
    }

    #[test]
    fn wiped_storage_is_discarded() {
        let address = H256::random();
        let db_storage =
            BTreeMap::from_iter((1..11).map(|key| (H256::from_low_u64_be(key), U256::from(key))));
        let post_state_storage =
            BTreeMap::from_iter((11..21).map(|key| (H256::from_low_u64_be(key), U256::from(key))));

        let db = create_test_rw_db();
        db.update(|tx| {
            for (slot, value) in db_storage {
                // insert zero value accounts to the database
                tx.put::<tables::HashedStorage>(address, StorageEntry { key: slot, value })
                    .unwrap();
            }
        })
        .unwrap();

        let wiped = true;
        let mut hashed_storage = HashedStorage::new(wiped);
        for (slot, value) in post_state_storage.iter() {
            hashed_storage.insert_non_zero_valued_storage(*slot, *value);
        }

        let mut hashed_post_state = HashedPostState::default();
        hashed_post_state.insert_hashed_storage(address, hashed_storage);
        hashed_post_state.sort();

        let tx = db.tx().unwrap();
        let factory = HashedPostStateCursorFactory::new(&tx, &hashed_post_state);
        let expected = [(address, post_state_storage)].into_iter();
        assert_storage_cursor_order(&factory, expected);
    }

    #[test]
    fn post_state_storages_take_precedence() {
        let address = H256::random();
        let storage =
            BTreeMap::from_iter((1..10).map(|key| (H256::from_low_u64_be(key), U256::from(key))));

        let db = create_test_rw_db();
        db.update(|tx| {
            for (slot, _) in storage.iter() {
                // insert zero value accounts to the database
                tx.put::<tables::HashedStorage>(
                    address,
                    StorageEntry { key: *slot, value: U256::ZERO },
                )
                .unwrap();
            }
        })
        .unwrap();

        let wiped = false;
        let mut hashed_storage = HashedStorage::new(wiped);
        for (slot, value) in storage.iter() {
            hashed_storage.insert_non_zero_valued_storage(*slot, *value);
        }

        let mut hashed_post_state = HashedPostState::default();
        hashed_post_state.insert_hashed_storage(address, hashed_storage);
        hashed_post_state.sort();

        let tx = db.tx().unwrap();
        let factory = HashedPostStateCursorFactory::new(&tx, &hashed_post_state);
        let expected = [(address, storage)].into_iter();
        assert_storage_cursor_order(&factory, expected);
    }

    #[test]
    fn fuzz_hashed_storage_cursor() {
        proptest!(ProptestConfig::with_cases(10),
            |(
                db_storages: BTreeMap<H256, BTreeMap<H256, U256>>,
                post_state_storages: BTreeMap<H256, (bool, BTreeMap<H256, U256>)>
            )|
        {
            let db = create_test_rw_db();
            db.update(|tx| {
                for (address, storage) in db_storages.iter() {
                    for (slot, value) in storage {
                        let entry = StorageEntry { key: *slot, value: *value };
                        tx.put::<tables::HashedStorage>(*address, entry).unwrap();
                    }
                }
            })
            .unwrap();

            let mut hashed_post_state = HashedPostState::default();

            for (address, (wiped, storage)) in &post_state_storages {
                let mut hashed_storage = HashedStorage::new(*wiped);
                for (slot, value) in storage {
                    if *value == U256::ZERO {
                        hashed_storage.insert_zero_valued_slot(*slot);
                    } else {
                        hashed_storage.insert_non_zero_valued_storage(*slot, *value);
                    }
                }
                hashed_post_state.insert_hashed_storage(*address, hashed_storage);
            }

            hashed_post_state.sort();

            let mut expected = db_storages;
            // overwrite or remove accounts from the expected result
            for (key, (wiped, storage)) in post_state_storages {
                let entry = expected.entry(key).or_default();
                if wiped {
                    entry.clear();
                }
                entry.extend(storage);
            }

            let tx = db.tx().unwrap();
            let factory = HashedPostStateCursorFactory::new(&tx, &hashed_post_state);
            assert_storage_cursor_order(&factory, expected.into_iter());
        });
    }
}
