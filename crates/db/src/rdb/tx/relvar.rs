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

use std::collections::HashSet;

use moor_values::util::SliceRef;

use crate::rdb::tuples::{TupleError, TupleRef};
use crate::rdb::tx::transaction::Transaction;
use crate::rdb::RelationId;

/// A reference / handle / pointer to a relation, the actual operations are managed through the
/// transaction.
/// A more convenient handle tied to the lifetime of the transaction.
// TODO: see comments on BaseRelation. changes there will require changes here.
pub struct RelVar<'a> {
    pub(crate) tx: &'a Transaction,
    pub(crate) id: RelationId,
}

impl<'a> RelVar<'a> {
    /// Seek for a tuple by its indexed domain value.
    pub fn seek_by_domain(&self, domain: SliceRef) -> Result<TupleRef, TupleError> {
        self.tx.seek_by_domain(self.id, domain)
    }

    /// Seek for tuples by their indexed codomain value, if there's an index. Panics if there is no
    /// secondary index.
    pub fn seek_by_codomain(&self, codomain: SliceRef) -> Result<HashSet<TupleRef>, TupleError> {
        self.tx.seek_by_codomain(self.id, codomain)
    }

    /// Insert a tuple into the relation.
    pub fn insert_tuple(&self, domain: SliceRef, codomain: SliceRef) -> Result<(), TupleError> {
        self.tx.insert_tuple(self.id, domain, codomain)
    }

    /// Update a tuple in the relation.
    pub fn update_tuple(&self, domain: SliceRef, codomain: SliceRef) -> Result<(), TupleError> {
        self.tx.update_tuple(self.id, domain, codomain)
    }

    /// Upsert a tuple into the relation.
    pub fn upsert_tuple(&self, domain: SliceRef, codomain: SliceRef) -> Result<(), TupleError> {
        self.tx.upsert_tuple(self.id, domain, codomain)
    }

    /// Remove a tuple from the relation.
    pub fn remove_by_domain(&self, domain: SliceRef) -> Result<(), TupleError> {
        self.tx.remove_by_domain(self.id, domain)
    }

    pub fn predicate_scan<F: Fn(&TupleRef) -> bool>(
        &self,
        f: &F,
    ) -> Result<Vec<TupleRef>, TupleError> {
        self.tx.predicate_scan(self.id, f)
    }
}
