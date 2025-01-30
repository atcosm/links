use super::{ActionableEvent, LinkReader, LinkStorage, PagedAppendingCollection};
use anyhow::{bail, Result};
use bincode::Options as BincodeOptions;
use link_aggregator::{Did, RecordId};
use links::CollectedLink;
use rocksdb::{
    AsColumnFamilyRef, ColumnFamilyDescriptor, DBWithThreadMode, IteratorMode, MergeOperands,
    MultiThreaded, Options, PrefixRange, ReadOptions, WriteBatch,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::marker::PhantomData;
use std::path::Path;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

static DID_IDS_CF: &str = "did_ids";
static TARGET_IDS_CF: &str = "target_ids";
static TARGET_LINKERS_CF: &str = "target_links";
static LINK_TARGETS_CF: &str = "link_targets";

static JETSTREAM_CURSOR_KEY: &str = "jetstream_cursor";

// todo: actually understand and set these options probably better
fn rocks_opts_base() -> Options {
    let mut opts = Options::default();
    opts.set_level_compaction_dynamic_level_bytes(true);
    opts.create_if_missing(true);
    opts.set_compression_type(rocksdb::DBCompressionType::Lz4);
    opts.set_bottommost_compression_type(rocksdb::DBCompressionType::Zstd);
    opts
}
fn get_db_opts() -> Options {
    let mut opts = rocks_opts_base();
    opts.create_missing_column_families(true);
    opts
}

#[derive(Debug, Clone)]
pub struct RocksStorage {
    db: Arc<DBWithThreadMode<MultiThreaded>>, // TODO: mov seqs here (concat merge op will be fun)
    did_id_table: IdTable<Did, DidIdValue, true>,
    target_id_table: IdTable<TargetKey, TargetId, false>,
    is_writer: bool,
}

trait IdTableValue: ValueFromRocks + Clone {
    fn new(v: u64) -> Self;
    fn id(&self) -> u64;
}
#[derive(Debug, Clone)]
struct IdTableBase<Orig, IdVal: IdTableValue>
where
    Orig: KeyFromRocks,
    for<'a> &'a Orig: AsRocksKey,
{
    _key_marker: PhantomData<Orig>,
    _val_marker: PhantomData<IdVal>,
    did_init: bool,
    name: String,
    id_seq: Arc<AtomicU64>,
}
impl<Orig, IdVal: IdTableValue> IdTableBase<Orig, IdVal>
where
    Orig: KeyFromRocks,
    for<'a> &'a Orig: AsRocksKey,
{
    fn cf_descriptor(&self) -> ColumnFamilyDescriptor {
        ColumnFamilyDescriptor::new(&self.name, rocks_opts_base())
    }
    fn init<const WITH_REVERSE: bool>(
        mut self,
        db: &DBWithThreadMode<MultiThreaded>,
    ) -> Result<IdTable<Orig, IdVal, WITH_REVERSE>> {
        if db.cf_handle(&self.name).is_none() {
            bail!("failed to get cf handle from db -- was the db open with our .cf_descriptor()?");
        }
        let priv_id_seq = if let Some(seq_bytes) = db.get(self.seq_key())? {
            if seq_bytes.len() != 8 {
                bail!(
                    "reading bytes for u64 id seq {:?}: found the wrong number of bytes",
                    self.seq_key()
                );
            }
            let mut buf: [u8; 8] = [0; 8];
            seq_bytes.as_slice().read_exact(&mut buf)?;
            let last_seq = u64::from_le_bytes(buf);
            last_seq + 1
        } else {
            1
        };
        self.id_seq.store(priv_id_seq, Ordering::SeqCst);
        self.did_init = true;
        Ok(IdTable {
            base: self,
            priv_id_seq,
        })
    }
    fn seq_key(&self) -> Vec<u8> {
        let mut k = b"__id_seq_key_plz_be_unique:".to_vec();
        k.extend(self.name.as_bytes());
        k
    }
}
impl<O, I: IdTableValue> Drop for IdTableBase<O, I>
where
    O: KeyFromRocks,
    for<'a> &'a O: AsRocksKey,
{
    fn drop(&mut self) {
        if !std::thread::panicking() && !self.did_init {
            panic!(
                "the id table '{}' was dropped without being initialized: call .init() on it.",
                self.name
            );
        }
    }
}
#[derive(Debug, Clone)]
struct IdTable<Orig, IdVal: IdTableValue, const WITH_REVERSE: bool>
where
    Orig: KeyFromRocks,
    for<'a> &'a Orig: AsRocksKey,
{
    base: IdTableBase<Orig, IdVal>,
    priv_id_seq: u64,
}
impl<Orig: Clone, IdVal: IdTableValue, const WITH_REVERSE: bool> IdTable<Orig, IdVal, WITH_REVERSE>
where
    Orig: KeyFromRocks,
    for<'v> &'v IdVal: AsRocksValue,
    for<'k> &'k Orig: AsRocksKey,
{
    #[must_use]
    fn setup(name: &str) -> IdTableBase<Orig, IdVal> {
        IdTableBase::<Orig, IdVal> {
            _key_marker: PhantomData,
            _val_marker: PhantomData,
            did_init: false,
            name: name.into(),
            id_seq: Arc::new(AtomicU64::new(0)), // zero is "uninint", first seq num will be 1
        }
    }
    fn get_id_val(
        &self,
        db: &DBWithThreadMode<MultiThreaded>,
        orig: &Orig,
    ) -> Result<Option<IdVal>> {
        let cf = db.cf_handle(&self.base.name).unwrap();
        if let Some(_id_bytes) = db.get_cf(&cf, _rk(orig))? {
            Ok(Some(_vr(&_id_bytes)?))
        } else {
            Ok(None)
        }
    }
    fn __get_or_create_id_val<CF>(
        &mut self,
        cf: &CF,
        db: &DBWithThreadMode<MultiThreaded>,
        batch: &mut WriteBatch,
        orig: &Orig,
    ) -> Result<IdVal>
    where
        CF: AsColumnFamilyRef,
    {
        Ok(self.get_id_val(db, orig)?.unwrap_or_else(|| {
            let prev_priv_seq = self.priv_id_seq;
            self.priv_id_seq += 1;
            let prev_public_seq = self.base.id_seq.swap(self.priv_id_seq, Ordering::SeqCst);
            assert_eq!(
                prev_public_seq, prev_priv_seq,
                "public seq may have been modified??"
            );
            let id_value = IdVal::new(self.priv_id_seq);
            batch.put(self.base.seq_key(), self.priv_id_seq.to_le_bytes());
            batch.put_cf(cf, _rk(orig), _rv(&id_value));
            id_value
        }))
    }
}
impl<Orig: Clone, IdVal: IdTableValue> IdTable<Orig, IdVal, true>
where
    Orig: KeyFromRocks,
    for<'v> &'v IdVal: AsRocksValue,
    for<'k> &'k Orig: AsRocksKey,
{
    fn get_or_create_id_val(
        &mut self,
        db: &DBWithThreadMode<MultiThreaded>,
        batch: &mut WriteBatch,
        orig: &Orig,
    ) -> Result<IdVal> {
        let cf = db.cf_handle(&self.base.name).unwrap();
        let id_val = self.__get_or_create_id_val(&cf, db, batch, orig)?;
        // TODO: assert that the original is never a u64 that could collide
        batch.put_cf(&cf, id_val.id().to_be_bytes(), _rk(orig)); // reversed rk/rv on purpose here :/
        Ok(id_val)
    }

    fn get_val_from_id(
        &self,
        db: &DBWithThreadMode<MultiThreaded>,
        id: u64,
    ) -> Result<Option<Orig>> {
        let cf = db.cf_handle(&self.base.name).unwrap();
        if let Some(orig_bytes) = db.get_cf(&cf, id.to_be_bytes())? {
            // HACK ish
            Ok(Some(_kr(&orig_bytes)?))
        } else {
            Ok(None)
        }
    }
}
impl<Orig: Clone, IdVal: IdTableValue> IdTable<Orig, IdVal, false>
where
    Orig: KeyFromRocks,
    for<'v> &'v IdVal: AsRocksValue,
    for<'k> &'k Orig: AsRocksKey,
{
    fn get_or_create_id_val(
        &mut self,
        db: &DBWithThreadMode<MultiThreaded>,
        batch: &mut WriteBatch,
        orig: &Orig,
    ) -> Result<IdVal> {
        let cf = db.cf_handle(&self.base.name).unwrap();
        self.__get_or_create_id_val(&cf, db, batch, orig)
    }
}

impl IdTableValue for DidIdValue {
    fn new(v: u64) -> Self {
        DidIdValue(DidId(v), true)
    }
    fn id(&self) -> u64 {
        self.0 .0
    }
}
impl IdTableValue for TargetId {
    fn new(v: u64) -> Self {
        TargetId(v)
    }
    fn id(&self) -> u64 {
        self.0
    }
}

impl RocksStorage {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let did_id_table = IdTable::<_, _, true>::setup(DID_IDS_CF);
        let target_id_table = IdTable::<_, _, false>::setup(TARGET_IDS_CF);

        let db = DBWithThreadMode::open_cf_descriptors(
            &get_db_opts(),
            path,
            vec![
                // id reference tables
                did_id_table.cf_descriptor(),
                target_id_table.cf_descriptor(),
                // the reverse links:
                ColumnFamilyDescriptor::new(TARGET_LINKERS_CF, {
                    let did_id_seq = did_id_table.id_seq.clone();
                    let mut opts = rocks_opts_base();
                    opts.set_merge_operator_associative(
                        "merge_op_extend_did_ids",
                        move |k, ex, ops| Self::merge_op_extend_did_ids(k, ex, ops, &did_id_seq),
                    );
                    opts
                }),
                // unfortunately we also need forward links to handle deletes
                ColumnFamilyDescriptor::new(LINK_TARGETS_CF, rocks_opts_base()),
            ],
        )?;
        let db = Arc::new(db);
        let did_id_table = did_id_table.init(&db)?;
        let target_id_table = target_id_table.init(&db)?;
        Ok(Self {
            db,
            did_id_table,
            target_id_table,
            is_writer: true,
        })
    }

    fn merge_op_extend_did_ids(
        _new_key: &[u8],
        existing: Option<&[u8]>,
        operands: &MergeOperands,
        current_did_id_seq: &AtomicU64,
    ) -> Option<Vec<u8>> {
        let mut tls: TargetLinkers = existing
            .map(|existing_bytes| _vr(existing_bytes).unwrap())
            .unwrap_or_default();

        let current_seq = current_did_id_seq.load(Ordering::SeqCst);

        for did_id_rkey in &tls.0 {
            let DidId(ref n) = did_id_rkey.0;
            if current_seq > 0 && *n > current_seq {
                eprintln!("problem with merge_op_extend_did_ids. existing: {tls:?}");
                eprintln!(
                    "an entry has did_id={n}, which is higher than the current sequence: {current_seq}"
                );
                panic!("got a did to merge with higher-than-current did_id sequence");
            }
        }

        for op in operands {
            let new_linkers: TargetLinkers = _vr(op).unwrap();
            for (DidId(ref n), _) in &new_linkers.0 {
                if current_seq > 0 && *n > current_seq {
                    let orig: Option<TargetLinkers> =
                        existing.map(|existing_bytes| _vr(existing_bytes).unwrap());
                    eprintln!(
                        "problem with merge_op_extend_did_ids. existing: {orig:?}\nnew linkers: {new_linkers:?}"
                    );
                    eprintln!("the current sequence is {current_seq}");
                    panic!("did_id a did to a number higher than the current sequence");
                }
            }
            tls.0.extend(new_linkers.0);
        }
        Some(_rv(&tls))
    }

    fn prefix_iter_cf<K, V, CF, P>(
        &self,
        cf: &CF,
        pre: P,
    ) -> impl Iterator<Item = (K, V)> + use<'_, K, V, CF, P>
    where
        K: KeyFromRocks,
        V: ValueFromRocks,
        CF: AsColumnFamilyRef,
        for<'a> &'a P: AsRocksKeyPrefix<K>,
    {
        let mut read_opts = ReadOptions::default();
        read_opts.set_iterate_range(PrefixRange(_rkp(&pre))); // TODO verify: inclusive bounds?
        self.db
            .iterator_cf_opt(cf, read_opts, IteratorMode::Start)
            .map_while(Result::ok)
            .map_while(|(k, v)| Some((_kr(&k).ok()?, _vr(&v).ok()?)))
    }

    fn update_did_id_value<F>(&self, batch: &mut WriteBatch, did: &Did, update: F) -> Result<bool>
    where
        F: FnOnce(DidIdValue) -> Option<DidIdValue>,
    {
        let cf = self.db.cf_handle(DID_IDS_CF).unwrap();
        let Some(did_id_value) = self.did_id_table.get_id_val(&self.db, did)? else {
            return Ok(false);
        };
        let Some(new_did_id_value) = update(did_id_value) else {
            return Ok(false);
        };
        batch.put_cf(&cf, _rk(did), _rv(&new_did_id_value));
        Ok(true)
    }
    fn delete_did_id_value(&self, batch: &mut WriteBatch, did: &Did) {
        let cf = self.db.cf_handle(DID_IDS_CF).unwrap();
        batch.delete_cf(&cf, _rk(did));
    }

    fn get_target_linkers(&self, target_id: &TargetId) -> Result<TargetLinkers> {
        let cf = self.db.cf_handle(TARGET_LINKERS_CF).unwrap();
        let Some(linkers_bytes) = self.db.get_cf(&cf, _rk(target_id))? else {
            return Ok(TargetLinkers::default());
        };
        _vr(&linkers_bytes)
    }
    fn merge_target_linker(
        &self,
        batch: &mut WriteBatch,
        target_id: &TargetId,
        linker_did_id: &DidId,
        linker_rkey: &RKey,
    ) {
        let cf = self.db.cf_handle(TARGET_LINKERS_CF).unwrap();
        batch.merge_cf(
            &cf,
            _rk(target_id),
            _rv(&TargetLinkers(vec![(*linker_did_id, linker_rkey.clone())])),
        );
    }
    fn update_target_linkers<F>(
        &self,
        batch: &mut WriteBatch,
        target_id: &TargetId,
        update: F,
    ) -> Result<bool>
    where
        F: FnOnce(TargetLinkers) -> Option<TargetLinkers>,
    {
        let cf = self.db.cf_handle(TARGET_LINKERS_CF).unwrap();
        let existing_linkers = self.get_target_linkers(target_id)?;
        let Some(new_linkers) = update(existing_linkers) else {
            return Ok(false);
        };
        batch.put_cf(&cf, _rk(target_id), _rv(&new_linkers));
        Ok(true)
    }

    fn put_link_targets(
        &self,
        batch: &mut WriteBatch,
        record_link_key: &RecordLinkKey,
        targets: &RecordLinkTargets,
    ) {
        // todo: we are almost idempotent to link creates with this blind write, but we'll still
        // merge in the reverse index. we could read+modify+write here but it'll be SLOWWWWW on
        // the path that we need to be fast. we could go back to a merge op and probably be
        // consistent. or we can accept just a littttttle inconsistency and be idempotent on
        // forward links but not reverse, slightly messing up deletes :/
        // _maybe_ we could run in slow idempotent r-m-w mode during firehose catch-up at the start,
        // then switch to the fast version?
        let cf = self.db.cf_handle(LINK_TARGETS_CF).unwrap();
        batch.put_cf(&cf, _rk(record_link_key), _rv(targets));
    }
    fn get_record_link_targets(
        &self,
        record_link_key: &RecordLinkKey,
    ) -> Result<Option<RecordLinkTargets>> {
        let cf = self.db.cf_handle(LINK_TARGETS_CF).unwrap();
        if let Some(bytes) = self.db.get_cf(&cf, _rk(record_link_key))? {
            Ok(Some(_vr(&bytes)?))
        } else {
            Ok(None)
        }
    }
    fn delete_record_link(&self, batch: &mut WriteBatch, record_link_key: &RecordLinkKey) {
        let cf = self.db.cf_handle(LINK_TARGETS_CF).unwrap();
        batch.delete_cf(&cf, _rk(record_link_key));
    }
    fn iter_links_for_did_id(
        &self,
        did_id: &DidId,
    ) -> impl Iterator<Item = (RecordLinkKey, RecordLinkTargets)> + use<'_> {
        let cf = self.db.cf_handle(LINK_TARGETS_CF).unwrap();
        self.prefix_iter_cf(&cf, RecordLinkKeyDidIdPrefix(*did_id))
    }
    fn iter_targets_for_target(
        &self,
        target: &Target,
    ) -> impl Iterator<Item = (TargetKey, TargetId)> + use<'_> {
        let cf = self.db.cf_handle(TARGET_IDS_CF).unwrap();
        self.prefix_iter_cf(&cf, TargetIdTargetPrefix(target.clone()))
    }

    fn check_for_did_dups(&self, problem_did_id: &DidId) {
        let cf = self.db.cf_handle(DID_IDS_CF).unwrap();
        let mut seen_ids: HashMap<DidId, Vec<Did>> = HashMap::new();
        for (k, v) in self
            .db
            .iterator_cf(&cf, rocksdb::IteratorMode::Start)
            .map_while(Result::ok)
        {
            let did: Did = _kr(&k).unwrap();
            let DidIdValue(did_id, _) = _vr(&v).unwrap();
            if let Some(dids) = seen_ids.get_mut(&did_id) {
                let is_problem = did_id == *problem_did_id;
                eprintln!(
                    "dup! problem? {is_problem}. at did_id {did_id:?}: {dids:?} and now {did:?}"
                );
                dids.push(did);
            } else {
                if did_id == *problem_did_id {
                    eprintln!("found our friend {did:?} {did_id:?}");
                }
                assert!(seen_ids.insert(did_id, vec![did]).is_none());
            }
        }
        eprintln!("done checking.");
    }

    //
    // higher-level event action handlers
    //

    fn add_links(&mut self, record_id: &RecordId, links: &[CollectedLink], batch: &mut WriteBatch) {
        let DidIdValue(did_id, _) = self
            .did_id_table
            .get_or_create_id_val(&self.db, batch, &record_id.did)
            .unwrap();

        let record_link_key = RecordLinkKey(
            did_id,
            Collection(record_id.collection()),
            RKey(record_id.rkey()),
        );
        let mut record_link_targets = RecordLinkTargets::with_capacity(links.len());

        for CollectedLink { target, path } in links {
            let target_key = TargetKey(
                Target(target.clone().into_string()),
                Collection(record_id.collection()),
                RPath(path.clone()),
            );
            let target_id = self
                .target_id_table
                .get_or_create_id_val(&self.db, batch, &target_key)
                .unwrap();
            self.merge_target_linker(batch, &target_id, &did_id, &RKey(record_id.rkey()));

            record_link_targets.add(RecordLinkTarget(RPath(path.clone()), target_id))
        }

        self.put_link_targets(batch, &record_link_key, &record_link_targets);
    }

    fn remove_links(&mut self, record_id: &RecordId, batch: &mut WriteBatch) {
        let Some(DidIdValue(linking_did_id, did_active)) = self
            .did_id_table
            .get_id_val(&self.db, &record_id.did)
            .unwrap()
        else {
            return; // we don't know her: nothing to do
        };
        if !did_active {
            eprintln!(
                "removing links from apparently-inactive did {:?}",
                &record_id.did
            );
        }

        let record_link_key = RecordLinkKey(
            linking_did_id,
            Collection(record_id.collection()),
            RKey(record_id.rkey()),
        );
        let Some(record_link_targets) = self.get_record_link_targets(&record_link_key).unwrap()
        else {
            return; // we don't have these links
        };

        // we do read -> modify -> write here: could merge-op in the deletes instead?
        // otherwise it's another single-thread-constraining thing.
        for (i, RecordLinkTarget(rpath, target_id)) in record_link_targets.0.iter().enumerate() {
            self.update_target_linkers(batch, target_id, |mut linkers| {
                if linkers.0.is_empty() {
                    eprintln!("about to blow up because a linked target is apparently missing.");
                    eprintln!("removing links for: {record_id:?}");
                    eprintln!("found links: {record_link_targets:?}");
                    eprintln!("working on #{i}: {rpath:?} / {target_id:?}");
                    panic!("it was empty");
                }
                if !linkers.remove_linker(&linking_did_id, &RKey(record_id.rkey.clone())) {
                    eprintln!("about to blow up because a linked target apparently does not have us in its dids.");
                    eprintln!("removing links for: {record_id:?}");
                    eprintln!("found links: {record_link_targets:?}");
                    eprintln!("working on #{i}: {rpath:?} / {target_id:?}");
                    eprintln!("trying to find us ({linking_did_id:?}) in {linkers:?}");
                    panic!("reverse index didn't have us");
                }
                Some(linkers)
            }).unwrap();
        }

        self.delete_record_link(batch, &record_link_key);
    }

    fn update_links(
        &mut self,
        record_id: &RecordId,
        new_links: &[CollectedLink],
        batch: &mut WriteBatch,
    ) {
        self.remove_links(record_id, batch);
        self.add_links(record_id, new_links, batch);
    }

    fn set_account(&mut self, did: &Did, active: bool, batch: &mut WriteBatch) {
        // this needs to be read-modify-write since the did_id needs to stay the same,
        // which has a benefit of allowing to avoid adding entries for dids we don't
        // need. reading on dids needs to be cheap anyway for the current design, and
        // did active/inactive updates are low-freq in the firehose so, eh, it's fine.
        self.update_did_id_value(batch, did, |current_value| {
            Some(DidIdValue(current_value.did_id(), active))
        })
        .unwrap();
    }

    fn delete_account(&mut self, did: &Did, batch: &mut WriteBatch) {
        let Some(DidIdValue(did_id, active)) = self.did_id_table.get_id_val(&self.db, did).unwrap()
        else {
            return; // ignore updates for dids we don't know about
        };
        self.delete_did_id_value(batch, did);
        // TODO: also delete the reverse!!

        // use a separate batch for all their links, since it can be a lot and make us crash at around 1GiB batch size.
        // this should still hopefully be crash-safe: as long as we don't actually delete the DidId entry until after all links are cleared.
        // the above .delete_did_id_value is batched, so it shouldn't be written until we've returned from this fn successfully
        // TODO: queue a background delete task or whatever
        // TODO: test delete account with more links than chunk size
        let stuff: Vec<_> = self.iter_links_for_did_id(&did_id).collect();
        for chunk in stuff.chunks(1024) {
            let mut mini_batch = WriteBatch::default();

            for (i, (record_link_key, links)) in chunk.iter().enumerate() {
                self.delete_record_link(&mut mini_batch, record_link_key); // _could_ use delete range here instead of individual deletes, but since we have to scan anyway it's not obvious if it's better

                for (j, RecordLinkTarget(path, target_link_id)) in links.0.iter().enumerate() {
                    self.update_target_linkers(&mut mini_batch, target_link_id, |mut linkers| {
                        if !linkers.remove_linker(&did_id, &record_link_key.2) {
                            eprintln!(
                                "DELETING ACCOUNT: blowing up: missing linker entry in linked target."
                            );
                            eprintln!("account: {did:?}");
                            eprintln!("did_id: {did_id:?}, was active? {active:?}");
                            eprintln!("with links: {links:?}");
                            eprintln!("and linkers: {linkers:?}");
                            eprintln!(
                                "working on #{i}.#{j}: {:?} / {path:?} / {target_link_id:?}",
                                record_link_key.collection()
                            );
                            eprintln!("from record link key {record_link_key:?}");
                            eprintln!("but could not find this link :/");
                            eprintln!("checking for did_id dups...");
                            self.check_for_did_dups(&did_id);
                            eprintln!("ok so what the heck. did_id again, for did {did:?}:");
                            let did_id_again = self
                                .did_id_table
                                .get_id_val(&self.db, did)
                                .unwrap()
                                .unwrap();
                            eprintln!("did_id_value (again): {did_id_again:?}");
                            panic!("ohnoooo");
                        }
                        Some(linkers)
                    })
                    .unwrap();
                }
            }

            self.db.write(mini_batch).unwrap(); // todo
        }
    }
}

impl Drop for RocksStorage {
    fn drop(&mut self) {
        if self.is_writer {
            println!("rocksdb writer: cleaning up for shutdown...");
            if let Err(e) = self.db.flush_wal(true) {
                eprintln!("rocks: flushing wal failed: {e:?}");
            }
            if let Err(e) = self.db.flush_opt(&{
                let mut opt = rocksdb::FlushOptions::default();
                opt.set_wait(true);
                opt
            }) {
                eprintln!("rocks: flushing memtables failed: {e:?}");
            }
            self.db.cancel_all_background_work(true);
        }
    }
}

impl AsRocksValue for u64 {}
impl ValueFromRocks for u64 {}

impl LinkStorage for RocksStorage {
    fn get_cursor(&mut self) -> Result<Option<u64>> {
        self.db
            .get(JETSTREAM_CURSOR_KEY)?
            .map(|b| _vr(&b))
            .transpose()
    }

    fn push(&mut self, event: &ActionableEvent, cursor: u64) -> Result<()> {
        let mut batch = WriteBatch::default();
        match event {
            ActionableEvent::CreateLinks { record_id, links } => {
                self.add_links(record_id, links, &mut batch)
            }
            ActionableEvent::UpdateLinks {
                record_id,
                new_links,
            } => self.update_links(record_id, new_links, &mut batch),
            ActionableEvent::DeleteRecord(record_id) => self.remove_links(record_id, &mut batch),
            ActionableEvent::ActivateAccount(did) => self.set_account(did, true, &mut batch),
            ActionableEvent::DeactivateAccount(did) => self.set_account(did, false, &mut batch),
            ActionableEvent::DeleteAccount(did) => self.delete_account(did, &mut batch),
        }
        batch.put(JETSTREAM_CURSOR_KEY.as_bytes(), _rv(cursor));
        self.db.write(batch)?;
        Ok(())
    }

    fn to_readable(&mut self) -> impl LinkReader {
        let mut readable = self.clone();
        readable.is_writer = false;
        readable
    }
}

impl LinkReader for RocksStorage {
    fn summarize(&self, qsize: u32) {
        let did_seq = self.did_id_table.base.id_seq.load(Ordering::SeqCst);
        let target_seq = self.target_id_table.base.id_seq.load(Ordering::SeqCst);
        println!("queue: {qsize}. did seq: {did_seq}, target seq: {target_seq}.");
    }

    fn get_count(&self, target: &str, collection: &str, path: &str) -> Result<u64> {
        let target_key = TargetKey(
            Target(target.to_string()),
            Collection(collection.to_string()),
            RPath(path.to_string()),
        );
        if let Some(target_id) = self.target_id_table.get_id_val(&self.db, &target_key)? {
            let (alive, _) = self.get_target_linkers(&target_id)?.count();
            Ok(alive)
        } else {
            Ok(0)
        }
    }

    fn get_links(
        &self,
        target: &str,
        collection: &str,
        path: &str,
        limit: u64,
        until: Option<u64>,
    ) -> Result<PagedAppendingCollection<RecordId>> {
        let target_key = TargetKey(
            Target(target.to_string()),
            Collection(collection.to_string()),
            RPath(path.to_string()),
        );

        let Some(target_id) = self.target_id_table.get_id_val(&self.db, &target_key)? else {
            return Ok(PagedAppendingCollection {
                version: (0, 0),
                items: Vec::new(),
                next: None,
            });
        };

        let linkers = self.get_target_linkers(&target_id)?;

        let (alive, gone) = linkers.count();
        let total = alive + gone;
        let end = until.map(|u| std::cmp::min(u, total)).unwrap_or(total) as usize;
        let begin = end.saturating_sub(limit as usize);
        let next = if begin == 0 { None } else { Some(begin as u64) };

        let did_id_rkeys = linkers.0[begin..end].iter().rev().collect::<Vec<_>>();

        let mut items = Vec::with_capacity(did_id_rkeys.len());
        // TODO: use get-many (or multi-get or whatever it's called)
        for (did_id, rkey) in did_id_rkeys {
            if did_id.is_empty() {
                continue;
            }
            if let Some(did) = self.did_id_table.get_val_from_id(&self.db, did_id.0)? {
                let Some(DidIdValue(_, active)) = self.did_id_table.get_id_val(&self.db, &did)?
                else {
                    eprintln!("failed to look up did_value from did_id {did_id:?}: {did:?}: data consistency bug?");
                    continue;
                };
                if !active {
                    continue;
                }
                items.push(RecordId {
                    did,
                    collection: collection.to_string(),
                    rkey: rkey.0.clone(),
                });
            } else {
                eprintln!("failed to look up did from did_id {did_id:?}");
            }
        }

        Ok(PagedAppendingCollection {
            version: (total, gone),
            items,
            next,
        })
    }

    fn get_all_counts(&self, target: &str) -> Result<HashMap<String, HashMap<String, u64>>> {
        let mut out: HashMap<String, HashMap<String, u64>> = HashMap::new();
        for (target_key, target_id) in self.iter_targets_for_target(&Target(target.into())) {
            let TargetKey(_, Collection(ref collection), RPath(ref path)) = target_key;
            let (count, _) = self.get_target_linkers(&target_id)?.count();
            out.entry(collection.into())
                .or_default()
                .insert(path.clone(), count);
        }
        Ok(out)
    }
}

trait AsRocksKey: Serialize {}
trait AsRocksKeyPrefix<K: KeyFromRocks>: Serialize {}
trait AsRocksValue: Serialize {}
trait KeyFromRocks: for<'de> Deserialize<'de> {}
trait ValueFromRocks: for<'de> Deserialize<'de> {}

// did_id table
impl AsRocksKey for &Did {}
impl AsRocksValue for &DidIdValue {}
impl ValueFromRocks for DidIdValue {}

// temp
impl KeyFromRocks for Did {}
impl AsRocksKey for &DidId {}

// target_ids table
impl AsRocksKey for &TargetKey {}
impl AsRocksKeyPrefix<TargetKey> for &TargetIdTargetPrefix {}
impl AsRocksValue for &TargetId {}
impl KeyFromRocks for TargetKey {}
impl ValueFromRocks for TargetId {}

// target_links table
impl AsRocksKey for &TargetId {}
impl AsRocksValue for &TargetLinkers {}
impl ValueFromRocks for TargetLinkers {}

// record_link_targets table
impl AsRocksKey for &RecordLinkKey {}
impl AsRocksKeyPrefix<RecordLinkKey> for &RecordLinkKeyDidIdPrefix {}
impl AsRocksValue for &RecordLinkTargets {}
impl KeyFromRocks for RecordLinkKey {}
impl ValueFromRocks for RecordLinkTargets {}

fn _bincode_opts() -> impl BincodeOptions {
    bincode::DefaultOptions::new().with_big_endian() // happier db -- numeric prefixes in lsm
}
fn _rk(k: impl AsRocksKey) -> Vec<u8> {
    _bincode_opts().serialize(&k).unwrap()
}
fn _rkp<K: KeyFromRocks>(kp: impl AsRocksKeyPrefix<K>) -> Vec<u8> {
    _bincode_opts().serialize(&kp).unwrap()
}
fn _rv(v: impl AsRocksValue) -> Vec<u8> {
    _bincode_opts().serialize(&v).unwrap()
}
fn _kr<T: KeyFromRocks>(bytes: &[u8]) -> Result<T> {
    Ok(_bincode_opts().deserialize(bytes)?)
}
fn _vr<T: ValueFromRocks>(bytes: &[u8]) -> Result<T> {
    Ok(_bincode_opts().deserialize(bytes)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Collection(String);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RPath(String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct RKey(String);

impl RKey {
    fn empty() -> Self {
        RKey("".to_string())
    }
}

// did ids
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct DidId(u64);

impl DidId {
    fn empty() -> Self {
        DidId(0)
    }
    fn is_empty(&self) -> bool {
        self.0 == 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DidIdValue(DidId, bool); // active or not

impl DidIdValue {
    fn did_id(&self) -> DidId {
        let Self(id, _) = self;
        *id
    }
}

// target ids
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TargetId(u64); // key

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Target(String); // the actual target/uri

// targets (uris, dids, etc.): the reverse index
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TargetKey(Target, Collection, RPath);

#[derive(Debug, Default, Serialize, Deserialize)]
struct TargetLinkers(Vec<(DidId, RKey)>);

impl TargetLinkers {
    fn remove_linker(&mut self, did: &DidId, rkey: &RKey) -> bool {
        if let Some(entry) = self.0.iter_mut().rfind(|d| **d == (*did, rkey.clone())) {
            *entry = (DidId::empty(), RKey::empty());
            true
        } else {
            false
        }
    }
    fn count(&self) -> (u64, u64) {
        // (linkers, deleted links)
        let total = self.0.len() as u64;
        let alive = self.0.iter().filter(|(DidId(id), _)| *id != 0).count() as u64;
        let gone = total - alive;
        (alive, gone)
    }
}

// forward links to targets so we can delete links
#[derive(Debug, Serialize, Deserialize)]
struct RecordLinkKey(DidId, Collection, RKey);

impl RecordLinkKey {
    fn collection(&self) -> Collection {
        let Self(_, collection, _) = self;
        collection.clone()
    }
}

// does this even work????
#[derive(Debug, Serialize, Deserialize)]
struct RecordLinkKeyDidIdPrefix(DidId);

#[derive(Debug, Serialize, Deserialize)]
struct TargetIdTargetPrefix(Target);

#[derive(Debug, Serialize, Deserialize)]
struct RecordLinkTarget(RPath, TargetId);

#[derive(Debug, Default, Serialize, Deserialize)]
struct RecordLinkTargets(Vec<RecordLinkTarget>);

impl RecordLinkTargets {
    fn with_capacity(cap: usize) -> Self {
        Self(Vec::with_capacity(cap))
    }
    fn add(&mut self, target: RecordLinkTarget) {
        self.0.push(target)
    }
}

#[cfg(test)]
mod tests {
    use super::super::ActionableEvent;
    use super::*;
    use links::Link;
    use tempfile::tempdir;

    #[test]
    fn rocks_delete_iterator_regression() -> Result<()> {
        let mut store = RocksStorage::new(tempdir()?)?;

        // create a link from the deleter account
        store.push(
            &ActionableEvent::CreateLinks {
                record_id: RecordId {
                    did: "did:plc:will-shortly-delete".into(),
                    collection: "a.b.c".into(),
                    rkey: "asdf".into(),
                },
                links: vec![CollectedLink {
                    target: Link::Uri("example.com".into()),
                    path: ".uri".into(),
                }],
            },
            0,
        )?;

        // and a different link from a separate, new account (later in didid prefix iteration)
        store.push(
            &ActionableEvent::CreateLinks {
                record_id: RecordId {
                    did: "did:plc:someone-else".into(),
                    collection: "a.b.c".into(),
                    rkey: "asdf".into(),
                },
                links: vec![CollectedLink {
                    target: Link::Uri("another.example.com".into()),
                    path: ".uri".into(),
                }],
            },
            0,
        )?;

        // now delete the first account (this is where the buggy version explodes)
        store.push(
            &ActionableEvent::DeleteAccount("did:plc:will-shortly-delete".into()),
            0,
        )?;

        Ok(())
    }

    #[test]
    fn rocks_prefix_iteration_helper() -> Result<()> {
        #[derive(Serialize, Deserialize)]
        struct Key(u8, u8);

        #[derive(Serialize)]
        struct KeyPrefix(u8);

        #[derive(Serialize, Deserialize)]
        struct Value(());

        impl AsRocksKey for &Key {}
        impl AsRocksKeyPrefix<Key> for &KeyPrefix {}
        impl AsRocksValue for &Value {}

        impl KeyFromRocks for Key {}
        impl ValueFromRocks for Value {}

        let data = RocksStorage::new(tempdir()?)?;
        let cf = data.db.cf_handle(DID_IDS_CF).unwrap();
        let mut batch = WriteBatch::default();

        // not our prefix
        batch.put_cf(&cf, _rk(&Key(0x01, 0x00)), _rv(&Value(())));
        batch.put_cf(&cf, _rk(&Key(0x01, 0xFF)), _rv(&Value(())));

        // our prefix!
        for i in 0..=0xFF {
            batch.put_cf(&cf, _rk(&Key(0x02, i)), _rv(&Value(())));
        }

        // not our prefix
        batch.put_cf(&cf, _rk(&Key(0x03, 0x00)), _rv(&Value(())));
        batch.put_cf(&cf, _rk(&Key(0x03, 0xFF)), _rv(&Value(())));

        data.db.write(batch)?;

        let mut okays: [bool; 256] = [false; 256];
        for (i, (k, Value(_))) in data.prefix_iter_cf(&cf, KeyPrefix(0x02)).enumerate() {
            assert!(i < 256);
            assert_eq!(k.0, 0x02, "prefix iterated key has the right prefix");
            assert_eq!(k.1 as usize, i, "prefixed keys are iterated in exact order");
            okays[k.1 as usize] = true;
        }
        assert!(okays.iter().all(|b| *b), "every key was iterated");

        Ok(())
    }

    // TODO: add tests for key prefixes actually prefixing (bincode encoding _should_...)
}
