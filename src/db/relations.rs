use std::collections::{BTreeMap, BTreeSet, Bound, HashMap, HashSet};
use std::marker::PhantomData;
use std::sync::atomic::AtomicU64;

use hybrid_lock::HybridLock;
use rkyv::ser::serializers::{AlignedSerializer, CompositeSerializer};
use rkyv::ser::Serializer;
use rkyv::{AlignedVec, Archive, Deserialize, Serialize};
use thiserror::Error;

use crate::db::relations::Error::{Conflict, NotFound};
use crate::db::tx::{CommitCheckResult, EntryValue, MvccEntry, MvccTuple, Tx};

#[derive(Error, Debug, Eq, PartialEq)]
pub enum Error {
    #[error("tuple not found for key")]
    NotFound,
    #[error("duplicate tuple")]
    Duplicate,
    #[error("commit conflict, abort transaction & retry")]
    Conflict,
}

pub trait SerializationTraits:
    rkyv::Serialize<
    CompositeSerializer<
        AlignedSerializer<AlignedVec>,
        rkyv::ser::serializers::FallbackScratch<
            rkyv::ser::serializers::HeapScratch<0>,
            rkyv::ser::serializers::AllocScratch,
        >,
        rkyv::ser::serializers::SharedSerializeMap,
    >,
>
{
}
impl<
        T: rkyv::Serialize<
            CompositeSerializer<
                AlignedSerializer<AlignedVec>,
                rkyv::ser::serializers::FallbackScratch<
                    rkyv::ser::serializers::HeapScratch<0>,
                    rkyv::ser::serializers::AllocScratch,
                >,
                rkyv::ser::serializers::SharedSerializeMap,
            >,
        >,
    > SerializationTraits for T
{
}

pub trait TupleValueTraits: Clone + Eq + PartialEq + Ord + Archive + SerializationTraits {}
impl<T: Clone + Eq + PartialEq + Ord + Archive + SerializationTraits> TupleValueTraits for T {}

#[derive(
    Debug, Serialize, Deserialize, Archive, Ord, PartialOrd, Copy, Clone, Eq, PartialEq, Hash,
)]
#[archive(compare(PartialEq), check_bytes)]
#[archive_attr(derive(Ord, PartialOrd, Copy, Clone, Eq, PartialEq, Hash,))]
pub struct TupleId(u64);

// The inner state that can be locked.
struct RelationInner<L: TupleValueTraits, R: TupleValueTraits> {
    // Tuple storage for this relation.
    // Right now this is a hash mapping tuple IDs to the tuple values.
    // There are likely much faster data structures for this.
    values: HashMap<TupleId, MvccTuple<TupleId, (L, R)>>,

    // Indexes for the L and (optionally) R attributes.
    l_index: BTreeMap<L, TupleId>,
    r_index: Option<BTreeMap<R, HashSet<TupleId>>>,

    // The commit-set per transaction id. Holds the set of dirtied tuple IDs to be managed at commit
    // time.
    // Hashtable for now, but can revisit later.
    commit_sets: HashMap<u64, Vec<TupleId>>,
}

impl<L: TupleValueTraits, R: TupleValueTraits> RelationInner<L, R> {
    fn add_to_commit_set(&mut self, tx: &mut Tx, tuple_id: TupleId) {
        self.commit_sets
            .entry(tx.tx_id)
            .and_modify(|c| c.push(tuple_id))
            .or_insert(vec![tuple_id]);
    }
}

// Describes a sort of specialized 2-ary relation, where L and R are the types of the two 'columns'.
// Indexes can exist for both L and R columns, but must always exist for L.
// The tuple values are stored in the indexes.
pub struct Relation<L: TupleValueTraits, R: TupleValueTraits> {
    next_tuple_id: AtomicU64,

    inner: HybridLock<RelationInner<L, R>>,
}

impl<L: TupleValueTraits, R: TupleValueTraits> Default for Relation<L, R> {
    fn default() -> Self {
        Relation::new()
    }
}

impl<L: TupleValueTraits, R: TupleValueTraits> Relation<L, R> {
    pub fn new() -> Self {
        let inner = RelationInner {
            values: Default::default(),
            l_index: Default::default(),
            r_index: None,
            commit_sets: Default::default(),
        };
        Relation {
            next_tuple_id: Default::default(),
            inner: HybridLock::new(inner),
        }
    }

    pub fn new_bidirectional() -> Self {
        let inner = RelationInner {
            values: Default::default(),
            l_index: Default::default(),
            r_index: Some(Default::default()),
            commit_sets: Default::default(),
        };
        Relation {
            next_tuple_id: Default::default(),
            inner: HybridLock::new(inner),
        }
    }

    fn has_with_l(&self, tx: &Tx, k: &L) -> bool {
        let inner = self.inner.read();
        if let Some(tuple_id) = inner.l_index.get(k).cloned() {
            let (_rts, value) = inner.values.get(&tuple_id).unwrap().get(tx.tx_start_ts);
            return value.is_some();
        }
        false
    }

    pub fn insert(&mut self, tx: &mut Tx, l: &L, r: &R) -> Result<(), Error> {
        let mut inner = self.inner.write();

        // If there's already a tuple for this row, then we need to check if it's visible to us.
        let tuple_id = if let Some(tuple_id) = inner.l_index.get(l) {
            let tuple_id = *tuple_id;
            let tuple = inner.values.get_mut(&tuple_id).unwrap();
            let (rts, value) = tuple.get(tx.tx_start_ts);

            // A row visible to us? That's a duplicate.
            if let Some(_value) = value {
                return Err(Error::Duplicate);
            }

            // There's a value invisible to us that's not deleted, we will actually treat that as an
            // entirely different version, because it means someone got to that row before us, but
            // we don't know what they might do with it (they could roll back, etc.)
            // At commit time, this should get sorted out as a conflict, depending on who got there
            // first.
            tuple.set(tx.tx_id, rts, &(l.clone(), r.clone()));

            tuple_id
        } else {
            // Didn't exist for any transaction, so create a new version, stick in our WAL.
            let tuple_id = TupleId(
                self.next_tuple_id
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            );
            // Start with a tombstone, just to reserve the slot
            inner.values.insert(
                tuple_id,
                MvccTuple::new(tx.tx_start_ts, EntryValue::Tombstone),
            );
            inner.l_index.insert(l.clone(), tuple_id);

            let tuple = inner.values.get_mut(&tuple_id).unwrap();
            tuple.set(tx.tx_id, tx.tx_start_ts, &(l.clone(), r.clone()));

            tuple_id
        };

        // TODO versioning on secondary indexes is suspect.
        if let Some(r_index) = &mut inner.r_index {
            r_index
                .entry(r.clone())
                .or_insert_with(Default::default)
                .insert(tuple_id);
        }
        inner.add_to_commit_set(tx, tuple_id);

        Ok(())
    }

    pub fn upsert(&mut self, tx: &mut Tx, l: &L, r: &R) -> Result<(), Error> {
        let e = self.remove_for_l(tx, l);
        if e != Ok(()) && e != Err(NotFound) {
            return e;
        }
        self.insert(tx, l, r)?;

        Ok(())
    }

    pub fn remove_for_l(&mut self, tx: &mut Tx, l: &L) -> Result<(), Error> {
        let mut inner = self.inner.write();

        if let Some(tuple_id) = inner.l_index.get(l).cloned() {
            let tuple = inner.values.get_mut(&tuple_id).unwrap();
            let (rts, value) = tuple.get(tx.tx_start_ts);

            // If we already deleted it or it's not visible to us, we can't delete it.
            if value.is_none() {
                return Err(Error::NotFound);
            }

            tuple.delete(tx.tx_id, rts);
            inner.add_to_commit_set(tx, tuple_id);

            if let Some(r_index) = &mut inner.r_index {
                if let Some(value) = value {
                    r_index.entry(value.1).and_modify(|s| {
                        s.remove(&tuple_id);
                    });
                }
            }
            return Ok(());
        }

        Err(Error::NotFound)
    }

    pub fn update_l(&mut self, tx: &mut Tx, l: &L, new_l: &L) -> Result<(), Error> {
        let Some(current_r) = self.seek_for_l_eq(tx, l) else {
            return Err(Error::NotFound);
        };
        self.remove_for_l(tx, l)?;
        self.insert(tx, new_l, &current_r)?;

        Ok(())
    }

    pub fn update_r(&mut self, tx: &mut Tx, l: &L, new_r: &R) -> Result<(), Error> {
        let mut inner = self.inner.write();

        if let Some(tuple_id) = inner.l_index.get(l).cloned() {
            let tuple_id = tuple_id;

            let tuple = inner.values.get_mut(&tuple_id).unwrap();
            let (rts, value) = tuple.get(tx.tx_start_ts);

            // If it's deleted by us or invisible to us, we can't update it, can we.
            let Some(old_value) = value else {
                return Err(Error::NotFound);
            };

            tuple.set(tx.tx_id, rts, &(l.clone(), new_r.clone()));
            inner.add_to_commit_set(tx, tuple_id);

            // Update secondary index.
            // TODO: this is not versioned...
            if let Some(r_index) = &mut inner.r_index {
                r_index
                    .entry(old_value.1)
                    .or_insert_with(Default::default)
                    .remove(&tuple_id);
                r_index
                    .entry(new_r.clone())
                    .or_insert_with(Default::default)
                    .insert(tuple_id);
            }
            return Ok(());
        }

        Err(Error::NotFound)
    }

    pub fn seek_for_l_eq(&self, tx: &Tx, k: &L) -> Option<R> {
        let inner = self.inner.read();

        if let Some(tuple_id) = inner.l_index.get(k).cloned() {
            let tuple = inner.values.get(&tuple_id).unwrap();
            return tuple.get(tx.tx_start_ts).1.map(|v| v.1);
        }
        None
    }

    pub fn range_for_l_eq(&self, tx: &Tx, range: (Bound<&L>, Bound<&L>)) -> Vec<(L, R)> {
        let inner = self.inner.read();

        let tuple_range = inner.l_index.range(range);
        let visible_tuples = tuple_range.filter_map(|(k, tuple_id)| {
            let tuple = inner.values.get(tuple_id);
            if let Some(tuple) = tuple {
                let (_rts, value) = tuple.get(tx.tx_start_ts);
                if let Some(value) = value {
                    return Some((k.clone(), value.1));
                }
            };
            None
        });
        visible_tuples.collect()
    }

    pub fn seek_for_r_eq(&self, tx: &Tx, t: &R) -> BTreeSet<L> {
        let inner = self.inner.read();

        let Some(t_index) = &inner.r_index else {
            panic!("secondary index query without index");
        };

        match t_index.get(t) {
            None => BTreeSet::new(),
            Some(tuples) => {
                let visible_tuples = tuples.iter().filter_map(|tuple_id| {
                    let tuple = inner.values.get(tuple_id);
                    if let Some(tuple) = tuple {
                        let (_rts, value) = tuple.get(tx.tx_start_ts);
                        if let Some(value) = value {
                            return Some(value.0);
                        }
                    };
                    None
                });
                visible_tuples.collect()
            }
        }
    }

    pub fn begin(&mut self, tx: &mut Tx) -> Result<(), Error> {
        let mut inner = self.inner.write();
        inner.commit_sets.entry(tx.tx_id).or_default();
        Ok(())
    }

    pub fn commit(&mut self, tx: &mut Tx) -> Result<(), Error> {
        let mut inner = self.inner.write();

        // Flush the Tx's WAL writes to the main data structures.
        let commit_set = inner.commit_sets.get(&tx.tx_id).cloned();
        let Some(commit_set) = commit_set else {
            // No commit set for this transaction (probably means `begin` was not called, which is
            // a bit dubious.
            return Ok(())
        };

        let mut versions = vec![];

        let mut can_commit = true;
        for tuple_id in commit_set {
            let tuple = inner
                .values
                .get_mut(&tuple_id)
                .expect("tuple in commit set missing from relation");
            let result = tuple.can_commit(tx.tx_start_ts);
            match result {
                CommitCheckResult::CanCommit(version_offset) => {
                    versions.push((tuple_id, version_offset))
                }
                CommitCheckResult::Conflict => {
                    can_commit = false;
                }
                CommitCheckResult::None => continue,
            }
        }

        // If commit check failed, rollback, which will destroy our extant versions.
        if !can_commit {
            drop(inner);
            self.rollback(tx)?;
            return Err(Conflict);
        }

        // Do the actual commits.
        for (tuple_id, position) in versions {
            let tuple = inner
                .values
                .get_mut(&tuple_id)
                .expect("tuple in commit set missing from relation");
            tuple.do_commit(tx.tx_start_ts, position);
        }

        Ok(())
    }

    pub fn rollback(&mut self, tx: &mut Tx) -> Result<(), Error> {
        let mut inner = self.inner.write();

        // Rollback means we have to go delete all the versions created by us.
        // And throw away the commit sets for this tx.
        let Some(commit_set) = inner.commit_sets.remove(&tx.tx_id) else {
            return Ok(())
        };

        // Find this transactions versions and destroy them.
        for tuple_id in commit_set {
            let tuple = inner
                .values
                .get_mut(&tuple_id)
                .expect("tuple in commit set missing from relation");
            tuple.rollback(tx.tx_id);
        }

        drop(inner);

        Ok(())
    }

    pub fn vacuum(&mut self) -> Result<(), Error> {
        todo!("implement");
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Archive)]
pub struct PMvccTuple<K: TupleValueTraits, V: TupleValueTraits> {
    pub versions: Vec<MvccEntry<V>>,
    pd: PhantomData<K>,
}

#[derive(Serialize, Deserialize, Archive)]
pub struct PRelation<L: TupleValueTraits, R: TupleValueTraits> {
    values: Vec<(TupleId, PMvccTuple<TupleId, (L, R)>)>,
    next_tuple_id: AtomicU64,

    // Indexes for the L and (optionally) R attributes.
    l_index: BTreeMap<L, TupleId>,
    r_index: Option<BTreeMap<R, HashSet<TupleId>>>,
}

#[cfg(test)]
mod tests {
    use std::collections::Bound::{Included, Unbounded};

    use crate::db::relations::Error::Conflict;

    use super::*;

    #[test]
    fn insert_new_tuple() {
        let mut relation = Relation::<String, i32>::new();

        let mut tx1 = Tx::new(1, 1);
        assert_eq!(relation.insert(&mut tx1, &"hello".to_string(), &1), Ok(()));
        assert_eq!(relation.insert(&mut tx1, &"world".to_string(), &2), Ok(()));
    }

    #[test]
    fn insert_existing_tuple() {
        let mut relation = Relation::<String, i32>::new();

        let mut tx1 = Tx::new(1, 1);
        assert_eq!(relation.insert(&mut tx1, &"hello".to_string(), &1), Ok(()));
        assert_eq!(
            relation.insert(&mut tx1, &"hello".to_string(), &2),
            Err(Error::Duplicate)
        );
    }

    #[test]
    fn upsert_new_tuple() {
        let mut relation = Relation::<String, i32>::new();

        let mut tx1 = Tx::new(1, 1);
        assert_eq!(relation.upsert(&mut tx1, &"hello".to_string(), &1), Ok(()));
    }

    #[test]
    fn upsert_existing_tuple() {
        let mut relation = Relation::<String, i32>::new();

        let mut tx1 = Tx::new(1, 1);
        assert_eq!(relation.insert(&mut tx1, &"hello".to_string(), &1), Ok(()));
        assert_eq!(relation.upsert(&mut tx1, &"hello".to_string(), &2), Ok(()));
    }

    #[test]
    fn remove_existing_tuple() {
        let mut relation = Relation::<String, i32>::new();

        let mut tx1 = Tx::new(1, 1);
        assert_eq!(relation.insert(&mut tx1, &"hello".to_string(), &1), Ok(()));
        assert_eq!(
            relation.remove_for_l(&mut tx1, &"hello".to_string()),
            Ok(())
        );
    }

    #[test]
    fn remove_nonexistent_tuple() {
        let mut relation = Relation::<String, i32>::new();
        let mut tx1 = Tx::new(1, 1);
        assert_eq!(
            relation.remove_for_l(&mut tx1, &"hello".to_string()),
            Err(Error::NotFound)
        );
    }

    #[test]
    fn test_secondary_index() {
        let mut relation = Relation::<String, i32>::new_bidirectional();
        let mut tx1 = Tx::new(1, 1);

        assert_eq!(relation.insert(&mut tx1, &"hello".to_string(), &1), Ok(()));
        assert_eq!(relation.insert(&mut tx1, &"bye".to_string(), &1), Ok(()));
        assert_eq!(
            relation.insert(&mut tx1, &"tomorrow".to_string(), &2),
            Ok(())
        );
        assert_eq!(
            relation.insert(&mut tx1, &"yesterday".to_string(), &2),
            Ok(())
        );

        assert_eq!(
            relation.seek_for_r_eq(&mut tx1, &1),
            BTreeSet::from(["hello".into(), "bye".into()])
        );
        assert_eq!(
            relation.seek_for_r_eq(&mut tx1, &2),
            BTreeSet::from(["tomorrow".into(), "yesterday".into()])
        );

        assert_eq!(
            relation.update_l(&mut tx1, &"hello".to_string(), &"everyday".to_string()),
            Ok(())
        );
        assert_eq!(
            relation.seek_for_r_eq(&mut tx1, &1),
            BTreeSet::from(["everyday".into(), "bye".into()])
        );

        assert_eq!(
            relation.remove_for_l(&mut tx1, &"everyday".to_string()),
            Ok(())
        );
        assert_eq!(
            relation.seek_for_r_eq(&mut tx1, &1),
            BTreeSet::from(["bye".into()])
        );

        assert_eq!(relation.upsert(&mut tx1, &"bye".to_string(), &3), Ok(()));
        assert_eq!(relation.seek_for_r_eq(&mut tx1, &1), BTreeSet::from([]));
        assert_eq!(
            relation.seek_for_r_eq(&mut tx1, &3),
            BTreeSet::from(["bye".into()])
        );
        assert_eq!(relation.update_r(&mut tx1, &"bye".to_string(), &4), Ok(()));
        assert_eq!(
            relation.seek_for_r_eq(&mut tx1, &4),
            BTreeSet::from(["bye".into()])
        );
        assert_eq!(relation.seek_for_r_eq(&mut tx1, &3), BTreeSet::from([]));

        assert_eq!(
            relation.range_for_l_eq(&mut tx1, (Included(&"tomorrow".into()), Unbounded)),
            vec![("tomorrow".into(), 2), ("yesterday".into(), 2)]
        );
    }

    #[test]
    fn insert_transactional() {
        let mut a = Relation::<String, i32>::new();

        let mut s = Tx::new(1, 1);
        assert_eq!(a.insert(&mut s, &"hello".to_string(), &1), Ok(()));
        assert_eq!(a.commit(&mut s), Ok(()));

        let mut t1 = Tx::new(2, 2);
        assert_eq!(a.update_r(&mut t1, &"hello".to_string(), &2), Ok(()));

        let mut t2 = Tx::new(3, 3);
        assert_eq!(a.update_r(&mut t2, &"hello".to_string(), &3), Ok(()));
        assert_eq!(a.commit(&mut t1), Ok(()));

        // should fail because t2 (ts 3) is trying to commit a change based on (ts 1) but the most
        // recent committed change is (ts 2)
        assert_eq!(a.commit(&mut t2), Err(Error::Conflict));
    }

    #[test]
    fn delete_transactional() {
        let mut a = Relation::<String, i32>::new();

        let mut s = Tx::new(1, 1);
        assert_eq!(a.insert(&mut s, &"hello".to_string(), &1), Ok(()));
        assert_eq!(a.commit(&mut s), Ok(()));

        let mut t1 = Tx::new(2, 2);
        assert_eq!(a.remove_for_l(&mut t1, &"hello".to_string()), Ok(()));

        let mut t2 = Tx::new(3, 3);
        assert_eq!(a.remove_for_l(&mut t2, &"hello".to_string()), Ok(()));

        assert_eq!(a.commit(&mut t1), Ok(()));
        assert_eq!(a.commit(&mut t2), Err(Error::Conflict));
    }

    #[test]
    fn insert_delete_transactional() {
        let mut a = Relation::<String, i32>::new();

        let mut s = Tx::new(1, 1);
        assert_eq!(a.insert(&mut s, &"hello".to_string(), &1), Ok(()));
        assert_eq!(a.commit(&mut s), Ok(()));

        let mut t1 = Tx::new(2, 2);
        assert_eq!(a.remove_for_l(&mut t1, &"hello".to_string()), Ok(()));

        // the delete done by t1 hasn't been committed yet, so this is a duplicate and can't
        // be inserted.
        let mut t2 = Tx::new(3, 3);
        assert_eq!(
            a.insert(&mut t2, &"hello".to_string(), &3),
            Err(Error::Duplicate)
        );
        assert!(a.rollback(&mut t2).is_ok());
        assert_eq!(a.commit(&mut t1), Ok(()));

        // now that t1 has been committed, this insert should succeed.
        let mut t3 = Tx::new(4, 4);
        assert_eq!(a.insert(&mut t3, &"hello".to_string(), &3), Ok(()));
        assert_eq!(a.commit(&mut t3), Ok(()));
    }

    #[test]
    fn update_delete_transactional() {
        let mut a = Relation::<String, i32>::new();

        let mut s = Tx::new(1, 1);
        assert_eq!(a.insert(&mut s, &"hello".to_string(), &1), Ok(()));
        assert_eq!(a.commit(&mut s), Ok(()));

        let mut t1 = Tx::new(2, 2);
        assert_eq!(a.update_r(&mut t1, &"hello".to_string(), &2), Ok(()));

        let mut t2 = Tx::new(3, 3);
        assert_eq!(a.remove_for_l(&mut t2, &"hello".to_string()), Ok(()));

        // T2 should return Conflict, because it tried to delete before t1 (which had earlier ts
        // committed. Write timestamp for t2's a.hello should be later than t1's.
        assert_eq!(a.commit(&mut t1), Ok(()));
        assert_eq!(a.commit(&mut t2), Err(Conflict));
    }

    #[test]
    fn insert_parallel() {
        let mut a = Relation::<String, i32>::new();

        let mut t1 = Tx::new(1, 1);
        assert_eq!(a.insert(&mut t1, &"hello".to_string(), &1), Ok(()));

        let mut t2 = Tx::new(2, 2);
        assert_eq!(a.insert(&mut t2, &"world".to_string(), &2), Ok(()));

        assert_eq!(a.commit(&mut t1), Ok(()));
        assert_eq!(a.commit(&mut t2), Ok(()));
    }

    #[test]
    fn delete_insert_parallel() {
        let mut a = Relation::<String, i32>::new();

        let mut t1 = Tx::new(1, 1);
        assert_eq!(a.insert(&mut t1, &"hello".to_string(), &1), Ok(()));

        let mut t2 = Tx::new(2, 2);
        assert_eq!(
            a.remove_for_l(&mut t2, &"hello".to_string()),
            Err(Error::NotFound)
        );

        let mut t3 = Tx::new(3, 3);
        assert_eq!(a.insert(&mut t3, &"hello".to_string(), &3), Ok(()));

        assert_eq!(a.commit(&mut t1), Ok(()));
        assert_eq!(a.commit(&mut t2), Ok(()));

        // this fails because the remove_for_l didn't succeed (invisible) and t1 already committed
        assert_eq!(a.commit(&mut t3), Err(Conflict));
    }

    #[test]
    fn update_delete_parallel() {
        let mut a = Relation::<String, i32>::new();

        let mut t1 = Tx::new(1, 1);
        assert_eq!(a.insert(&mut t1, &"hello".to_string(), &1), Ok(()));

        let mut t2 = Tx::new(2, 2);
        assert_eq!(
            a.remove_for_l(&mut t2, &"hello".to_string()),
            Err(Error::NotFound)
        );
        assert_eq!(a.commit(&mut t1), Ok(()));

        let mut t3 = Tx::new(3, 3);
        assert_eq!(a.update_r(&mut t3, &"hello".to_string(), &3), Ok(()));
        assert_eq!(a.commit(&mut t2), Ok(()));

        // this succeeds because t3 forked from t1's version, and t2 failed in its delete.
        assert_eq!(a.commit(&mut t3), Ok(()));
    }

    #[test]
    fn test_insert_order_conflict() {
        let mut a = Relation::<String, i32>::new();

        let mut t1 = Tx::new(1, 1);
        a.begin(&mut t1).unwrap();

        assert_eq!(a.insert(&mut t1, &"hello".to_string(), &1), Ok(()));

        let mut t2 = Tx::new(2, 2);
        a.begin(&mut t2).unwrap();
        assert_eq!(a.insert(&mut t2, &"hello".to_string(), &2), Ok(()));

        assert_eq!(a.commit(&mut t1), Ok(()));

        // T2 should be a conflict because t1 got there first, and we didn't know about the
        // tuple there at the time of our insert.
        assert_eq!(a.commit(&mut t2), Err(Conflict));

        let mut t3 = Tx::new(3, 3);
        assert_eq!(a.seek_for_l_eq(&mut t3, &"hello".to_string()), Some(1));
    }
}
