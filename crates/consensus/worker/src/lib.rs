// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) Telcoin, LLC
// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

#![warn(future_incompatible, nonstandard_style, rust_2018_idioms, rust_2021_compatibility)]

mod block_fetcher;
mod block_provider;
mod network;
pub mod quorum_waiter;
mod worker;

pub mod metrics;

pub use crate::{
    block_provider::BlockProvider,
    worker::{Worker, CHANNEL_CAPACITY},
};

/// The number of shutdown receivers to create on startup. We need one per component loop.
pub const NUM_SHUTDOWN_RECEIVERS: u64 = 26;
