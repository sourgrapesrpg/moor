// Copyright (C) 2024 Ryan Daum <ryan.daum@gmail.com>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, version 3.
//
// This program is distributed in the hope that it will be useful, but WITHOUT
// ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
// FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License along with
// this program. If not, see <https://www.gnu.org/licenses/>.
//

// TODO: add fixed-size slotted page impl for Sized items, should be way more efficient for the
//       most common case of fixed-size tuples.
// TODO: implement the ability to expire and page-out tuples based on LRU or random/second
//       chance eviction (ala leanstore). will require separate PageIds from Bids, and will
//       involve rewriting SlotPtr on the fly to point to a new page when restored.
//       SlotPtr will also get a new field for last-access-time, so that we can do our eviction
// TODO: store indexes in here, too (custom paged datastructure impl)
// TODO: verify locking/concurrency safety of this thing -- loom test, stateright, or jepsen, etc.
// TODO: there is still some really gross stuff in here about the management of free space in
//       pages in the allocator list. It's probably causing excessive fragmentation because we're
//       considering only the reported available "content" area when fitting slots, and there seems
//       to be a sporadic failure where we end up with a "Page not found" error in the allocator on
//       free, meaning the page was not found in the used pages list.
//       whether any of this is worth futzing with after the fixed-size impl is done, I don't know.
// TODO: rename me, _I_ am the tuplebox. The "slots" are just where my tuples get stored. tho once
//       indexes are in here, things will get confusing (everything here assumes pages hold tuples)

use std::cmp::max;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::{Arc, Mutex};

use sized_chunks::SparseChunk;
use thiserror::Error;
use tracing::error;

use crate::tuplebox::pool::{Bid, BufferPool, PagerError};
use crate::tuplebox::tuples::slot_ptr::SlotPtr;
pub use crate::tuplebox::tuples::slotted_page::SlotId;
use crate::tuplebox::tuples::slotted_page::{
    slot_index_overhead, slot_page_empty_size, SlottedPage,
};
use crate::tuplebox::tuples::{TupleId, TupleRef};
use crate::tuplebox::RelationId;

pub type PageId = usize;

/// A SlotBox is a collection of (variable sized) pages, each of which is a collection of slots, each of which is holds
/// dynamically sized tuples.
pub struct SlotBox {
    inner: Mutex<Inner>,
}

#[derive(Debug, Clone, Error)]
pub enum SlotBoxError {
    #[error("Page is full, cannot insert slot of size {0} with {1} bytes remaining")]
    BoxFull(usize, usize),
    #[error("Tuple not found at index {0}")]
    TupleNotFound(usize),
}

impl SlotBox {
    pub fn new(virt_size: usize) -> Self {
        let pool = BufferPool::new(virt_size).expect("Could not create buffer pool");
        let inner = Mutex::new(Inner::new(pool));
        Self { inner }
    }

    /// Allocates a new slot for a tuple, somewhere in one of the pages we managed.
    /// Does not allow tuples from different relations to mix on the same page.
    pub fn allocate(
        self: Arc<Self>,
        size: usize,
        relation_id: RelationId,
        initial_value: Option<&[u8]>,
    ) -> Result<TupleRef, SlotBoxError> {
        let mut inner = self.inner.lock().unwrap();

        inner.do_alloc(size, relation_id, initial_value, &self)
    }

    pub(crate) fn load_page<LF: FnMut(Pin<&mut [u8]>)>(
        self: Arc<Self>,
        relation_id: RelationId,
        id: PageId,
        mut lf: LF,
    ) -> Result<Vec<TupleRef>, SlotBoxError> {
        let mut inner = self.inner.lock().unwrap();

        // Re-allocate the page.
        let page = inner.do_restore_page(id).unwrap();

        // Find all the slots referenced in this page.
        let slot_ids = page.load(|buf| {
            lf(buf);
        });

        // Now make sure we have swizrefs for all of them.
        let mut refs = vec![];
        for (slot, buflen, addr) in slot_ids.into_iter() {
            let tuple_id = TupleId { page: id, slot };
            let swizref = Box::pin(SlotPtr::create(self.clone(), tuple_id, addr, buflen));
            inner.swizrefs.insert(tuple_id, swizref);
            let swizref = inner.swizrefs.get_mut(&tuple_id).unwrap();
            let sp = unsafe { Pin::into_inner_unchecked(swizref.as_mut()) };
            let ptr = sp as *mut SlotPtr;
            let tuple_ref = TupleRef::at_ptr(ptr);
            refs.push(tuple_ref);
        }
        // The allocator needs to know that this page is used.
        inner.do_mark_page_used(relation_id, page.available_content_bytes(), id);
        Ok(refs)
    }

    pub(crate) fn page_for<'a>(&self, id: PageId) -> Result<SlottedPage<'a>, SlotBoxError> {
        let inner = self.inner.lock().unwrap();
        inner.page_for(id)
    }

    pub fn upcount(&self, id: TupleId) -> Result<(), SlotBoxError> {
        let inner = self.inner.lock().unwrap();
        let page_handle = inner.page_for(id.page)?;
        page_handle.upcount(id.slot)
    }

    pub fn dncount(&self, id: TupleId) -> Result<(), SlotBoxError> {
        let mut inner = self.inner.lock().unwrap();
        let page_handle = inner.page_for(id.page)?;
        if page_handle.dncount(id.slot)? {
            inner.do_remove(id)?;
        }
        Ok(())
    }

    pub fn get(&self, id: TupleId) -> Result<Pin<&[u8]>, SlotBoxError> {
        let inner = self.inner.lock().unwrap();
        let page_handle = inner.page_for(id.page)?;

        let lock = page_handle.read_lock();

        let slc = lock.get_slot(id.slot)?;
        Ok(slc)
    }

    pub fn update(
        self: Arc<Self>,
        relation_id: RelationId,
        id: TupleId,
        new_value: &[u8],
    ) -> Result<Option<TupleRef>, SlotBoxError> {
        let new_tup = {
            let mut inner = self.inner.lock().unwrap();
            let mut page_handle = inner.page_for(id.page)?;

            // If the value size is the same as the old value, we can just update in place, otherwise
            // it's a brand new allocation, and we have to remove the old one first.
            let mut page_write = page_handle.write_lock();
            let mut existing = page_write.get_slot_mut(id.slot).expect("Invalid tuple id");
            if existing.len() == new_value.len() {
                existing.copy_from_slice(new_value);
                return Ok(None);
            }
            inner.do_remove(id)?;

            inner.do_alloc(new_value.len(), relation_id, Some(new_value), &self)?
        };
        Ok(Some(new_tup))
    }

    pub fn update_with<F: FnMut(Pin<&mut [u8]>)>(
        &self,
        id: TupleId,
        mut f: F,
    ) -> Result<(), SlotBoxError> {
        let inner = self.inner.lock().unwrap();
        let mut page_handle = inner.page_for(id.page)?;
        let mut page_write = page_handle.write_lock();

        let existing = page_write.get_slot_mut(id.slot).expect("Invalid tuple id");

        f(existing);
        Ok(())
    }

    pub fn num_pages(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.available_page_space.len()
    }

    pub fn used_pages(&self) -> Vec<PageId> {
        let allocator = self.inner.lock().unwrap();
        allocator
            .available_page_space
            .iter()
            .map(|ps| ps.pages())
            .flatten()
            .collect()
    }
}

struct Inner {
    // TODO: buffer pool has its own locks per size class, so we might not need this inside another lock
    //   *but* the other two items here are not thread-safe, and we need to maintain consistency across the three.
    //   so we can maybe get rid of the locks in the buffer pool...
    pool: BufferPool,
    /// The set of used pages, indexed by relation, in sorted order of the free space available in them.
    available_page_space: SparseChunk<PageSpace, 64>,
    /// The "swizzelable" references to tuples, indexed by tuple id.
    /// There has to be a stable-memory address for each of these, as they are referenced by
    /// pointers in the TupleRefs themselves.
    // TODO: This needs to be broken down by page id, too, so that we can manage swap-in/swap-out at
    //   the page granularity.
    swizrefs: HashMap<TupleId, Pin<Box<SlotPtr>>>,
}

impl Inner {
    fn new(pool: BufferPool) -> Self {
        Self {
            available_page_space: SparseChunk::new(),
            pool,
            swizrefs: HashMap::new(),
        }
    }

    fn do_alloc(
        &mut self,
        size: usize,
        relation_id: RelationId,
        initial_value: Option<&[u8]>,
        sb: &Arc<SlotBox>,
    ) -> Result<TupleRef, SlotBoxError> {
        let tuple_size = size + slot_index_overhead();
        let page_size = max(32768, tuple_size.next_power_of_two());

        // Check if we have a free spot for this relation that can fit the tuple.
        let (page, offset) =
            { self.find_space(relation_id, tuple_size, slot_page_empty_size(page_size))? };

        let mut page_handle = self.page_for(page)?;

        let free_space = page_handle.available_content_bytes();
        let mut page_write_lock = page_handle.write_lock();
        if let Ok((slot, page_remaining, mut buf)) = page_write_lock.allocate(size, initial_value) {
            self.finish_alloc(page, relation_id, offset, page_remaining);

            // Make a swizzlable ptr reference and shove it in our set, and then return a tuple ref
            // which has a ptr to it.
            let buflen = buf.as_ref().len();
            let bufaddr = buf.as_mut_ptr();
            let tuple_id = TupleId { page, slot };

            // Heap allocate the swizref, and and pin it, take the address of it, then stick the swizref
            // in our set.
            let mut swizref = Box::pin(SlotPtr::create(sb.clone(), tuple_id, bufaddr, buflen));
            let swizaddr = unsafe { swizref.as_mut().get_unchecked_mut() } as *mut SlotPtr;
            self.swizrefs.insert(tuple_id, swizref);

            // Establish initial refcount using this existing lock.
            page_write_lock.upcount(slot).unwrap();

            return Ok(TupleRef::at_ptr(swizaddr));
        }

        // If we get here, then we failed to allocate on the page we wanted to, which means there's
        // data coherence issues between the pages last-reported free space and the actual free
        panic!(
            "Page {} failed to allocate, we wanted {} bytes, but it only has {},\
                but our records show it has {}, and its pid in that offset is {:?}",
            page,
            size,
            free_space,
            self.available_page_space[relation_id.0].available[offset],
            self.available_page_space[relation_id.0].block_ids[offset]
        );
    }

    fn do_restore_page<'a>(&mut self, id: PageId) -> Result<SlottedPage<'a>, SlotBoxError> {
        let (addr, page_size) = match self.pool.restore(Bid(id as u64)) {
            Ok(v) => v,
            Err(PagerError::CouldNotAccess) => {
                return Err(SlotBoxError::TupleNotFound(id));
            }
            Err(e) => {
                panic!("Unexpected buffer pool error: {:?}", e);
            }
        };

        Ok(SlottedPage::for_page(addr.load(SeqCst), page_size))
    }

    fn do_mark_page_used(&mut self, relation_id: RelationId, free_space: usize, pid: PageId) {
        let bid = Bid(pid as u64);
        let Some(available_page_space) = self.available_page_space.get_mut(relation_id.0) else {
            self.available_page_space
                .insert(relation_id.0, PageSpace::new(free_space, bid));
            return;
        };

        available_page_space.insert(free_space, bid);
    }

    fn do_remove(&mut self, id: TupleId) -> Result<(), SlotBoxError> {
        let mut page_handle = self.page_for(id.page)?;
        let mut write_lock = page_handle.write_lock();

        let (new_free, _, is_empty) = write_lock.remove_slot(id.slot)?;
        self.report_free(id.page, new_free, is_empty);

        // TODO: The swizref stays just in case?
        // self.swizrefs.remove(&id);

        Ok(())
    }

    fn page_for<'a>(&self, page_num: usize) -> Result<SlottedPage<'a>, SlotBoxError> {
        let (page_address, page_size) = match self.pool.resolve_ptr(Bid(page_num as u64)) {
            Ok(v) => v,
            Err(PagerError::CouldNotAccess) => {
                return Err(SlotBoxError::TupleNotFound(page_num));
            }
            Err(e) => {
                panic!("Unexpected buffer pool error: {:?}", e);
            }
        };
        let page_address = page_address.load(SeqCst);
        let page_handle = SlottedPage::for_page(page_address, page_size);
        Ok(page_handle)
    }

    fn alloc(
        &mut self,
        relation_id: RelationId,
        page_size: usize,
    ) -> Result<(PageId, usize), SlotBoxError> {
        // Ask the buffer pool for a new page of the given size.
        let (bid, _, actual_size) = match self.pool.alloc(page_size) {
            Ok(v) => v,
            Err(PagerError::InsufficientRoom { desired, available }) => {
                return Err(SlotBoxError::BoxFull(desired, available));
            }
            Err(e) => {
                panic!("Unexpected buffer pool error: {:?}", e);
            }
        };
        match self.available_page_space.get_mut(relation_id.0) {
            Some(available_page_space) => {
                available_page_space.insert(slot_page_empty_size(actual_size), bid);
                Ok((bid.0 as PageId, available_page_space.len() - 1))
            }
            None => {
                self.available_page_space.insert(
                    relation_id.0,
                    PageSpace::new(slot_page_empty_size(actual_size), bid),
                );
                Ok((bid.0 as PageId, 0))
            }
        }
    }

    /// Find room to allocate a new tuple of the given size, does not do the actual allocation yet,
    /// just finds the page to allocate it on.
    /// Returns the page id, and the offset into the `available_page_space` vector for that relation.
    fn find_space(
        &mut self,
        relation_id: RelationId,
        tuple_size: usize,
        page_size: usize,
    ) -> Result<(PageId, usize), SlotBoxError> {
        // Do we have a used pages set for this relation? If not, we can start one, and allocate a
        // new full page to it, and return. When we actually do the allocation, we'll be able to
        // find the page in the used pages set.
        let Some(available_page_space) = self.available_page_space.get_mut(relation_id.0) else {
            // Ask the buffer pool for a new buffer.
            return self.alloc(relation_id, page_size);
        };

        // Can we find some room?
        if let Some(found) = available_page_space.find_room(tuple_size) {
            return Ok(found);
        }

        // Out of room, need to allocate a new page.
        return self.alloc(relation_id, page_size);
    }

    fn finish_alloc(
        &mut self,
        _pid: PageId,
        relation_id: RelationId,
        offset: usize,
        page_remaining_bytes: usize,
    ) {
        let available_page_space = &mut self.available_page_space[relation_id.0];
        available_page_space.finish(offset, page_remaining_bytes);
    }

    fn report_free(&mut self, pid: PageId, new_size: usize, is_empty: bool) {
        // Seek the page in the available_page_space vectors, and add the bytes back to its free space.
        // We don't know the relation id here, so we have to linear scan all of them.
        for available_page_space in self.available_page_space.iter_mut() {
            if available_page_space.update_page(pid, new_size, is_empty) {
                if is_empty {
                    self.pool
                        .free(Bid(pid as u64))
                        .expect("Could not free page");
                }
                return;
            }
            return;
        }

        error!(
            "Page not found in used pages in allocator on free; pid {}; could be double-free, dangling weak reference?",
            pid
        );
    }
}

/// The amount of space available for each page known to the allocator for a relation.
/// Kept in two vectors, one for the available space, and one for the page ids, and kept sorted by
/// available space, with the page ids in the same order.
struct PageSpace {
    available: Vec<usize>,
    block_ids: Vec<Bid>,
}
impl PageSpace {
    fn new(available: usize, bid: Bid) -> Self {
        Self {
            available: vec![available],
            block_ids: vec![bid],
        }
    }

    fn sort(&mut self) {
        // sort both vectors by available space, keeping the block ids in order with the available
        let mut pairs = self
            .available
            .iter()
            .cloned()
            .zip(self.block_ids.iter())
            .collect::<Vec<_>>();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        self.available = pairs.iter().map(|(a, _)| *a).collect();
        self.block_ids = pairs.iter().map(|(_, b)| *b).cloned().collect();
    }

    fn insert(&mut self, available: usize, bid: Bid) {
        self.available.push(available);
        self.block_ids.push(bid);
        self.sort();
    }

    fn seek(&self, pid: PageId) -> Option<usize> {
        self.block_ids.iter().position(|bid| bid.0 == pid as u64)
    }

    /// Update the allocation record for the page.
    fn update_page(&mut self, pid: PageId, available: usize, is_empty: bool) -> bool {
        let Some(index) = self.seek(pid) else {
            return false;
        };

        // If the page is now totally empty, then we can remove it from the available_page_space vector.
        if is_empty {
            self.available.remove(index);
            self.block_ids.remove(index);
        } else {
            self.available[index] = available;
        }
        self.sort();
        true
    }

    /// Find which page in this relation has room for a tuple of the given size.
    fn find_room(&self, available: usize) -> Option<(PageId, usize)> {
        // Look for the first page with enough space in our vector of used pages, which is kept
        // sorted by free space.
        let found = self
            .available
            .binary_search_by(|free_space| free_space.cmp(&available));

        return match found {
            // Exact match, highly unlikely, but possible.
            Ok(entry_num) => {
                let exact_match = (self.block_ids[entry_num], entry_num);
                let pid = exact_match.0 .0 as PageId;
                Some((pid, entry_num))
            }
            // Out of room, our caller will need to allocate a new page.
            Err(position) if position == self.available.len() => {
                // If we didn't find a page with enough space, then we need to allocate a new page.
                return None;
            }
            // Found a page we add to.
            Err(entry_num) => {
                let page = self.block_ids[entry_num];
                Some((page.0 as PageId, entry_num))
            }
        };
    }

    fn finish(&mut self, offset: usize, page_remaining_bytes: usize) {
        self.available[offset] = page_remaining_bytes;

        // If we (unlikely) consumed all the bytes, then we can remove the page from the avail pages
        // set.
        if page_remaining_bytes == 0 {
            self.available.remove(offset);
            self.block_ids.remove(offset);
        }
        self.sort();
    }

    fn pages(&self) -> impl Iterator<Item = PageId> + '_ {
        self.block_ids.iter().map(|bid| bid.0 as PageId)
    }

    fn len(&self) -> usize {
        self.available.len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rand::distributions::Alphanumeric;
    use rand::{thread_rng, Rng};

    use crate::tuplebox::tuples::slotbox::{SlotBox, SlotBoxError};
    use crate::tuplebox::tuples::slotted_page::slot_page_empty_size;
    use crate::tuplebox::tuples::TupleRef;
    use crate::tuplebox::RelationId;

    fn fill_until_full(sb: &Arc<SlotBox>) -> Vec<(TupleRef, Vec<u8>)> {
        let mut tuples = Vec::new();

        // fill until full... (SlotBoxError::BoxFull)
        loop {
            let mut rng = thread_rng();
            let tuple_len = rng.gen_range(1..(slot_page_empty_size(52000)));
            let value: Vec<u8> = rng.sample_iter(&Alphanumeric).take(tuple_len).collect();
            match TupleRef::allocate(RelationId(0), sb.clone(), 0, &value, &value) {
                Ok(tref) => {
                    tuples.push((tref, value));
                }
                Err(SlotBoxError::BoxFull(_, _)) => {
                    break;
                }
                Err(e) => {
                    panic!("Unexpected error: {:?}", e);
                }
            }
        }
        tuples
    }

    // Just allocate a single tuple, and verify that we can retrieve it.
    #[test]
    fn test_one_page_one_slot() {
        let sb = Arc::new(SlotBox::new(32768 * 64));
        let expected_value = vec![1, 2, 3, 4, 5];
        let _retrieved = sb
            .clone()
            .allocate(expected_value.len(), RelationId(0), Some(&expected_value))
            .unwrap();
    }

    // Fill just one page and verify that we can retrieve them all.
    #[test]
    fn test_one_page_a_few_slots() {
        let sb = Arc::new(SlotBox::new(32768 * 64));
        let mut tuples = Vec::new();
        let mut last_page_id = None;
        loop {
            let mut rng = thread_rng();
            let tuple_len = rng.gen_range(1..128);
            let tuple: Vec<u8> = rng.sample_iter(&Alphanumeric).take(tuple_len).collect();
            let tuple_id = sb
                .clone()
                .allocate(tuple.len(), RelationId(0), Some(&tuple))
                .unwrap();
            if let Some(last_page_id) = last_page_id {
                if last_page_id != tuple_id.id() {
                    break;
                }
            }
            last_page_id = Some(tuple_id.id());
            tuples.push((tuple_id, tuple));
        }
        for (tuple, expected_value) in tuples {
            let retrieved = tuple.slot_buffer();
            assert_eq!(expected_value, retrieved.as_slice());
        }
    }

    // Fill one page, then overflow into another, and verify we can get the tuple that's on the next page.
    #[test]
    fn test_page_overflow() {
        let sb = Arc::new(SlotBox::new(32768 * 64));
        let mut tuples = Vec::new();
        let mut first_page_id = None;
        let (next_page_tuple_id, next_page_value) = loop {
            let mut rng = thread_rng();
            let tuple_len = rng.gen_range(1..128);
            let tuple: Vec<u8> = rng.sample_iter(&Alphanumeric).take(tuple_len).collect();
            let tuple_id = sb
                .clone()
                .allocate(tuple.len(), RelationId(0), Some(&tuple))
                .unwrap();
            if let Some(last_page_id) = first_page_id {
                if last_page_id != tuple_id.id() {
                    break (tuple_id, tuple);
                }
            }
            first_page_id = Some(tuple_id.id());
            tuples.push((tuple_id, tuple));
        };
        for (tuple, expected_value) in tuples {
            let retrieved = tuple.slot_buffer();
            assert_eq!(expected_value, retrieved.as_slice());
        }
        // Now verify that the last tuple was on another, new page, and that we can retrieve it.
        assert_ne!(next_page_tuple_id.id(), first_page_id.unwrap());
        let retrieved = next_page_tuple_id.slot_buffer();
        assert_eq!(retrieved.as_slice(), next_page_value);
    }

    // Generate a pile of random sized tuples (which accumulate to more than a single page size),
    // and then scan back and verify their presence/equality.
    #[test]
    fn test_basic_add_fill_etc() {
        let mut sb = Arc::new(SlotBox::new(32768 * 32));
        let mut tuples = fill_until_full(&mut sb);
        for (i, (tuple, expected_value)) in tuples.iter().enumerate() {
            let retrieved = tuple.domain();
            assert_eq!(
                *expected_value,
                retrieved.as_slice(),
                "Mismatch at {}th tuple",
                i
            );
        }
        let used_pages = sb.used_pages();
        assert_ne!(used_pages.len(), tuples.len());

        // Now free all the tuples. This will destroy their refcounts.
        tuples.clear();
    }

    // Verify that filling our box up and then emptying it out again works. Should end up with
    // everything mmap DONTNEED'd, and we should be able to re-fill it again, too.
    #[test]
    fn test_full_fill_and_empty() {
        let mut sb = Arc::new(SlotBox::new(32768 * 64));
        let mut tuples = fill_until_full(&mut sb);

        // Collect the manual ids of the tuples we've allocated, so we can check them for refcount goodness.
        let ids = tuples.iter().map(|(t, _)| t.id()).collect::<Vec<_>>();
        tuples.clear();

        // Verify that everything is gone.
        for id in ids {
            assert!(sb.get(id).is_err());
        }
    }

    // Fill a box with tuples, then go and free some random ones, verify their non-presence, then
    // fill back up again and verify the new presence.
    #[test]
    fn test_fill_and_free_and_refill_etc() {
        let mut sb = Arc::new(SlotBox::new(32768 * 64));
        let mut tuples = fill_until_full(&mut sb);
        let mut rng = thread_rng();
        let mut freed_tuples = Vec::new();

        // Pick a bunch of tuples at random to free, and remove them from the tuples set, which should dncount
        // them to 0, freeing them.
        let to_remove = tuples.len() / 2;
        for _ in 0..to_remove {
            let idx = rng.gen_range(0..tuples.len());
            let (tuple, value) = tuples.remove(idx);
            let id = tuple.id();
            freed_tuples.push((id, value));
        }

        // What we expected to still be there is there.
        for (tuple, expected_value) in &tuples {
            let retrieved = tuple.domain();
            assert_eq!(*expected_value, retrieved.as_slice());
        }
        // What we expected to not be there is not there.
        for (id, _) in freed_tuples {
            assert!(sb.get(id).is_err());
        }
        // Now fill back up again.
        let new_tuples = fill_until_full(&mut sb);
        // Verify both the new tuples and the old tuples are there.
        for (tuple, expected) in new_tuples {
            let retrieved = tuple.domain();
            assert_eq!(expected, retrieved.as_slice());
        }
        for (tuple, expected) in tuples {
            let retrieved = tuple.domain();
            assert_eq!(expected, retrieved.as_slice());
        }
    }
}
