// Deptool -- A declarative configuration deployment tool.
// Copyright 2026 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Deptool: a declarative deployment tool.
//!
//! Operator-side modules: [`plan`], [`deploy`], [`sync`], [`display`], [`setup`].
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
pub mod sync;

/// Test utilities, available to both unit tests and integration tests.
#[doc(hidden)]
pub mod testutil;
