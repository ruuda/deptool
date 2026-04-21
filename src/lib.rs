//! Deptool: a declarative deployment tool.
//!
//! Operator-side modules: [`plan`], [`deploy`], [`display`], [`setup`].
//! Agent-side modules: [`agent`], [`checkout`], [`log`].
//! Shared: [`store`], [`protocol`], [`error`], [`prim`].

pub mod agent;
pub mod checkout;
pub mod deploy;
pub mod display;
pub mod error;
pub mod log;
pub mod plan;
pub mod prim;
pub mod protocol;
pub mod setup;
pub mod store;

/// Test utilities, available to both unit tests and integration tests.
#[doc(hidden)]
pub mod testutil;
