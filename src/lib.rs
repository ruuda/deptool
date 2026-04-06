// Shared modules, re-exported for integration tests.
// The binary entry point is in main.rs.

mod apply;
pub mod deploy;
pub mod display;
pub mod error;
pub mod plan;
pub mod prim;
pub mod protocol;
pub mod session;
pub mod setup;
pub mod store;

/// Test utilities, available to both unit tests and integration tests.
#[doc(hidden)]
pub mod testutil;
