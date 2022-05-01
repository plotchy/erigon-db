use crate::kv::{
    tables::TableHandle,
    traits::{DefaultFlags, Mode, Table},
    EnvFlags, MdbxCursor, MdbxEnv, MdbxTx,
};
use ethereum_types::{Address, H256, H64, U256};
use eyre::{eyre, Result};
use fastrlp::{Decodable, Encodable};
use mdbx::{TransactionKind, RO, RW};

use tables::*;

pub mod models;
pub mod tables;

use models::{
    Account, BlockHeader, BlockNumber, BodyForStorage, Bytecode, HeaderKey, Incarnation, Rlp, PlainCodeKey,
    StorageHistKey, StorageKey,
};

pub const NUM_TABLES: usize = 50;
// https://github.com/ledgerwatch/erigon-lib/blob/625c9f5385d209dc2abfadedf6e4b3914a26ed3e/kv/mdbx/kv_mdbx.go#L154
pub const ENV_FLAGS: EnvFlags = EnvFlags {
    no_rdahead: true,
    coalesce: true,
    accede: true,
    no_sub_dir: false,
    exclusive: false,
    no_meminit: false,
    liforeclaim: false,
};

/// Erigon wraps an `MdbxTx` and provides Erigon-specific access methods.
pub struct Erigon<'env, K: TransactionKind>(pub MdbxTx<'env, K>);

impl<'env> Erigon<'env, RO> {
    pub fn open_ro(path: &std::path::Path) -> Result<MdbxEnv<RO>> {
        MdbxEnv::open_ro(path, NUM_TABLES, ENV_FLAGS)
    }
    pub fn begin(env: &'env MdbxEnv<RO>) -> Result<Self> {
        env.begin_ro().map(Self)
    }
}
impl<'env> Erigon<'env, RW> {
    pub fn open_rw(path: &std::path::Path) -> Result<MdbxEnv<RW>> {
        MdbxEnv::open_rw(path, NUM_TABLES, ENV_FLAGS)
    }
    pub fn begin_rw(env: &'env MdbxEnv<RW>) -> Result<Self> {
        env.begin_rw().map(Self)
    }
}
impl<'env, K: TransactionKind> Erigon<'env, K> {
    pub fn new(inner: MdbxTx<'env, K>) -> Self {
        Self(inner)
    }
}

impl<'env, K: Mode> Erigon<'env, K> {
    /// Opens and reads from the db table with the table's default flags
    pub fn read<'tx, T>(&'tx self, key: T::Key) -> Result<Option<T::Value>>
    where
        T: Table<'tx> + DefaultFlags,
    {
        self.0.get::<T, T::Flags>(self.0.open_db()?, key)
    }
    /// Opens a table with the table's default flags and creates a cursor into
    /// the opened table.
    pub fn cursor<'tx, T>(&'tx self) -> Result<MdbxCursor<'tx, K, T>>
    where
        T: Table<'tx> + DefaultFlags,
    {
        self.0.cursor::<T, T::Flags>(self.0.open_db()?)
    }

    /// Returns the hash of the current canonical head header.
    pub fn read_head_header_hash(&self) -> Result<Option<H256>> {
        self.read::<LastHeader>(LastHeader)
    }

    /// Returns the hash of the current canonical head block.
    pub fn read_head_block_hash(&self) -> Result<Option<H256>> {
        self.read::<LastBlock>(LastBlock)
    }

    /// Returns the incarnation of the account when it was last deleted.
    pub fn read_incarnation(&self, adr: Address) -> Result<Option<Incarnation>> {
        self.read::<IncarnationMap>(adr)
    }

    /// Returns the decoded account data as stored in the PlainState table.
    pub fn read_account_data(&self, adr: Address) -> Result<Option<Account>> {
        self.read::<PlainState>(adr)
    }

    /// Returns the number of the block containing the specified transaction.
    pub fn read_transaction_block_number(&self, hash: H256) -> Result<Option<U256>> {
        self.read::<BlockTransactionLookup>(hash)
    }

    /// Returns the block header identified by the (block number, block hash) key
    pub fn read_header(&self, key: HeaderKey) -> Result<Option<BlockHeader>> {
        self.read::<Header>(key)
    }

    /// Returns the decoding of the body as stored in the BlockBody table
    pub fn read_body_for_storage(&self, key: HeaderKey) -> Result<Option<BodyForStorage>> {
        self.read::<BlockBody>(key)?
            .map(|mut body| {
                // Skip 1 system tx at the beginning of the block and 1 at the end
                // https://github.com/ledgerwatch/erigon/blob/f56d4c5881822e70f65927ade76ef05bfacb1df4/core/rawdb/accessors_chain.go#L602-L605
                // https://github.com/ledgerwatch/erigon-lib/blob/625c9f5385d209dc2abfadedf6e4b3914a26ed3e/kv/tables.go#L28
                body.base_tx_id += 1;
                body.tx_amount = body.tx_amount.checked_sub(2).ok_or_else(|| {
                    eyre!(
                        "Block body has too few txs: {}. HeaderKey: {:?}",
                        body.tx_amount,
                        key,
                    )
                })?;
                Ok(body)
            })
            .transpose()
    }

    /// Returns the header number assigned to a hash.
    pub fn read_header_number(&self, hash: H256) -> Result<Option<BlockNumber>> {
        self.read::<HeaderNumber>(hash)
    }

    /// Returns the number of the current canonical block header.
    pub fn read_head_block_number(&self) -> Result<Option<BlockNumber>> {
        let hash = self.read_head_header_hash()?.ok_or(eyre!("No value"))?;
        self.read_header_number(hash)
    }

    /// Returns the signers of each transaction in the block.
    pub fn read_senders(&self, key: HeaderKey) -> Result<Option<Vec<Address>>> {
        self.read::<TxSender>(key)
    }

    /// Returns the hash assigned to a canonical block number.
    pub fn read_canonical_hash(&self, num: BlockNumber) -> Result<Option<H256>> {
        self.read::<CanonicalHeader>(num)
    }

    /// Determines whether a header with the given hash is on the canonical chain.
    pub fn is_canonical_hash(&self, hash: H256) -> Result<bool> {
        let num = self.read_header_number(hash)?.ok_or(eyre!("No value"))?;
        let canon = self.read_canonical_hash(num)?.ok_or(eyre!("No value"))?;
        Ok(canon != Default::default() && canon == hash)
    }

    /// Returns the value of the storage for account `adr` indexed by `key`.
    pub fn read_storage(&self, adr: Address, inc: Incarnation, key: H256) -> Result<Option<U256>> {
        let bucket = StorageKey::new(adr, inc);
        let mut cur = self.cursor::<Storage>()?;
        cur.seek_dup(bucket, key)
            .map(|kv| kv.and_then(|(k, v)| if k == key { Some(v) } else { None }))
    }

    /// Returns an iterator over all of the storage (key, value) pairs for the
    /// given address and account incarnation.
    pub fn walk_storage(
        &self,
        adr: Address,
        inc: Incarnation,
    ) -> Result<impl Iterator<Item = Result<(H256, U256)>>> {
        let start_key = StorageKey::new(adr, inc);
        self.cursor::<Storage>()?.walk_dup(start_key)
    }

    /// Returns the code associated with the given codehash.
    pub fn read_code(&self, codehash: H256) -> Result<Option<Bytecode>> {
        if codehash == models::EMPTY_HASH {
            return Ok(Default::default());
        }
        self.read::<Code>(codehash)
    }

    /// Returns the code associated with the given codehash.
    pub fn read_codehash(&self, adr: Address, inc: Incarnation) -> Result<Option<H256>> {
        let key = PlainCodeKey(adr, inc);
        self.read::<PlainCodeHash>(key)
    }

    // (address, block_num) => bitmap
    // from bitmap we extract the smallest block >= block_num where the account changed
    pub fn read_account_hist(&self, adr: Address, block: BlockNumber) -> Result<Option<Account>> {
        let mut hist_cur = self.cursor::<AccountHistory>()?;
        let mut cs_cur = self.cursor::<AccountChangeSet>()?;
        // The value from AccountHistory at the first key >= block.
        let (_, bitmap) = hist_cur.seek((adr, block))?.ok_or(eyre!("No value"))?;
        let cs_block = BlockNumber(find(bitmap, *block));
        // // look for cs_block, addresss
        if let Some((k, mut acct)) = cs_cur.seek_dup(cs_block, adr)? {
            if k == adr {
                if acct.incarnation > 0 && acct.codehash == Default::default() {
                    acct.codehash = self.read_codehash(adr, acct.incarnation)?.ok_or(eyre!("No value"))?
                }
                return Ok(Some(acct))
            }
        }
        Ok(None)
    }

    pub fn read_storage_hist(&self, adr: Address, inc: Incarnation, slot: H256, block: BlockNumber) -> Result<Option<U256>> {
        let mut hist_cur = self.cursor::<StorageHistory>()?;
        let mut cs_cur = self.cursor::<StorageChangeSet>()?;
        let (_, bitmap) = hist_cur.seek(StorageHistKey(adr, slot, block))?.ok_or(eyre!("No value"))?;
        let cs_block = BlockNumber(find(bitmap, *block));
        let cs_key = (cs_block, StorageKey::new(adr, inc));
        if let Some((k, v)) = cs_cur.seek_dup(cs_key, slot)? {
            if k == slot {
                return Ok(Some(v))
            }
        }
        Ok(None)
    }
}
use roaring::RoaringTreemap;
fn find(map: RoaringTreemap, n: u64) -> u64 {
    let rank = map.rank(n.saturating_sub(1));
    map.select(rank).unwrap()
}
// fn read_hist<'tx, K, TH, TC>(hist_cur: &mut MdbxCursor<'tx, K, TH>, changeset_cur: &mut MdbxCursor<'tx, K, TC>) where K: TransactionKind {

// }

impl<'env> Erigon<'env, mdbx::RW> {
    /// Opens and writes to the db table with the table's default flags.
    pub fn write<'tx, T>(&'tx self, key: T::Key, val: T::Value) -> Result<()>
    where
        T: Table<'tx> + DefaultFlags,
    {
        self.0.set::<T, T::Flags>(self.0.open_db()?, key, val)
    }

    pub fn write_head_header_hash(&self, v: H256) -> Result<()> {
        self.write::<LastHeader>(LastHeader, v)
    }
    pub fn write_head_block_hash(&self, v: H256) -> Result<()> {
        self.write::<LastBlock>(LastBlock, v)
    }
    pub fn write_incarnation(&self, k: Address, v: Incarnation) -> Result<()> {
        self.write::<IncarnationMap>(k, v)
    }
    pub fn write_account_data(&self, k: Address, v: Account) -> Result<()> {
        self.write::<PlainState>(k, v)
    }
    pub fn write_transaction_block_number(&self, k: H256, v: U256) -> Result<()> {
        self.write::<BlockTransactionLookup>(k, v)
    }
    pub fn write_header_number(&self, k: H256, v: BlockNumber) -> Result<()> {
        self.write::<HeaderNumber>(k, v)
    }
    pub fn write_header(&self, k: HeaderKey, v: BlockHeader) -> Result<()> {
        self.write::<Header>(k, v)
    }
    pub fn write_body_for_storage(&self, k: HeaderKey, v: BodyForStorage) -> Result<()> {
        self.write::<BlockBody>(k, v)
    }
}

// pub fn stream_transactions<'tx, K>(cur: &mut MdbxCursor<'tx, K, BlockTransaction>, start_key: u64) -> impl Iterator<Item= Result<>> where K: TransactionKind, {
//     todo!()
// }
// pub fn walk_storage<'tx, 'cur, K>(
//     cur: &'cur mut MdbxCursor<'tx, K, Storage>,
//     who: Address,
//     inc: Incarnation,
// ) -> Result<impl Iterator<Item = Result<(H256, U256)>> + 'cur>
// where
//     K: TransactionKind,
// {
//     let start_key = StorageKey::new(who, inc);
//     cur.walk_dup(start_key)
// }
