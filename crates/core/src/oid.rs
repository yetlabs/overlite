//! Object identifiers: how the protocol names a type.

use std::fmt;

/// A Postgres object identifier, the unsigned four bytes the protocol carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid(u32);

impl Oid {
    /// No type: what a parameter sends when it leaves the server to infer one.
    pub const UNSPECIFIED: Oid = Oid(0);

    #[must_use]
    pub const fn new(oid: u32) -> Oid {
        Oid(oid)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    #[must_use]
    pub const fn is_unspecified(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<u32> for Oid {
    fn from(oid: u32) -> Oid {
        Oid(oid)
    }
}

impl From<Oid> for u32 {
    fn from(oid: Oid) -> u32 {
        oid.0
    }
}
