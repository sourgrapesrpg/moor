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

//! In-memory database that provides transactional consistency through copy-on-write maps
//! Base relations are `im` hashmaps -- persistent / functional / copy-on-writish hashmaps, which
//! transactions obtain a fork of from `canonical`. At commit timestamps are checked and reconciled
//! if possible, and the whole set of relations is swapped out for the set of modified tuples.
//!
//! The tuples themselves are written out at commit time to a backing store, and then re-read at
//! system initialization.
//!
//! TLDR Transactions continue to see a fully snapshot isolated view of the world.

pub use relbox::{RelBox, RelationInfo};
pub use tuples::TupleError;
pub use tx::{CommitError, Transaction};

mod backing;
mod base_relation;

mod cold_storage;
mod page_storage;
mod pool;

mod paging;
mod relbox;
mod tuples;
mod tx;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct RelationId(pub usize);

impl RelationId {
    // If the top bit (63rd) bit is not set, then this is a base relation.
    pub fn is_base_relation(&self) -> bool {
        self.0 & (1 << 63) == 0
    }
    pub fn is_transient_relation(&self) -> bool {
        !self.is_base_relation()
    }
}
