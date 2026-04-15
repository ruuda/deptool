//! Deptool: a declarative deployment tool.
//!
//! Operator-side modules: [`plan`], [`deploy`], [`display`], [`setup`].
//! Agent-side modules: [`session`], [`checkout`].
//! Shared: [`store`], [`protocol`], [`error`], [`prim`].

pub mod checkout;
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
