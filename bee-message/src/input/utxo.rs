// Copyright 2020 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use crate::{output::OutputId, payload::transaction::TransactionId, Error};

use bee_common::packable::{Packable, Read, Write};

use core::{convert::From, str::FromStr};

#[derive(Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct UtxoInput(OutputId);

impl UtxoInput {
    pub const KIND: u8 = 0;

    pub fn new(id: TransactionId, index: u16) -> Result<Self, Error> {
        Ok(Self(OutputId::new(id, index)?))
    }

    pub fn output_id(&self) -> &OutputId {
        &self.0
    }
}

#[cfg(feature = "serde")]
string_serde_impl!(UtxoInput);

impl From<OutputId> for UtxoInput {
    fn from(id: OutputId) -> Self {
        UtxoInput(id)
    }
}

impl FromStr for UtxoInput {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(UtxoInput(OutputId::from_str(s)?))
    }
}

impl core::fmt::Display for UtxoInput {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl core::fmt::Debug for UtxoInput {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "UtxoInput({})", self.0)
    }
}

impl Packable for UtxoInput {
    type Error = Error;

    fn packed_len(&self) -> usize {
        self.0.packed_len()
    }

    fn pack<W: Write>(&self, writer: &mut W) -> Result<(), Self::Error> {
        self.0.pack(writer)
    }

    fn unpack<R: Read + ?Sized>(reader: &mut R) -> Result<Self, Self::Error> {
        Ok(Self(OutputId::unpack(reader)?))
    }
}
