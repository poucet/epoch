//! Errors raised when constructing a [`Tree`](crate::Tree).
//!
//! Decode errors do *not* live here — decoding flows through the kernel's
//! [`Codec`](epoch_cas::Codec) contract and so reports via
//! [`CASError::Codec`](epoch_cas::CASError). `TreeError` covers only the
//! in-memory construction invariants ([`Tree::from_entries`]).
//!
//! [`Tree::from_entries`]: crate::Tree::from_entries

/// An error from building a [`Tree`](crate::Tree).
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    /// Two entries shared the same `name`. A tree is a *set* keyed by name, so
    /// names must be unique.
    #[error("duplicate entry name: {name:?}")]
    DuplicateName {
        /// The repeated name's raw bytes.
        name: Vec<u8>,
    },
}
