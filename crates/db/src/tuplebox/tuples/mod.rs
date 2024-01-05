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

use thiserror::Error;

pub use slotbox::{PageId, SlotBox, SlotBoxError, SlotId};
pub use tuple::TupleRef;
pub use tx_tuple::TxTuple;

mod slot_ptr;
mod slotbox;
mod slotted_page;
mod tuple;
mod tx_tuple;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct TupleId {
    pub page: PageId,
    pub slot: SlotId,
}

#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum TupleError {
    #[error("Tuple not found")]
    NotFound,
    #[error("Tuple already exists")]
    Duplicate,
}
