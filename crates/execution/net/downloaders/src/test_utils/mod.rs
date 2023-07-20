#![allow(unused)]
//! Test helper impls
use crate::bodies::test_utils::create_raw_bodies;
use execution_interfaces::test_utils::generators::random_block_range;
use futures::SinkExt;
use std::{collections::HashMap, io::SeekFrom, ops::RangeInclusive};
use tn_types::execution::{BlockBody, SealedHeader, H256};
use tokio::{
    fs::File,
    io::{AsyncSeekExt, AsyncWriteExt, BufWriter},
};
use tokio_util::codec::FramedWrite;

mod bodies_client;
mod file_client;
mod file_codec;

pub use bodies_client::TestBodiesClient;
use execution_interfaces::test_utils::generators;
pub use file_client::{FileClient, FileClientError};
pub(crate) use file_codec::BlockFileCodec;

/// Metrics scope used for testing.
pub(crate) const TEST_SCOPE: &str = "downloaders.test";

/// Generate a set of bodies and their corresponding block hashes
pub(crate) fn generate_bodies(
    range: RangeInclusive<u64>,
) -> (Vec<SealedHeader>, HashMap<H256, BlockBody>) {
    let mut rng = generators::rng();
    let blocks = random_block_range(&mut rng, range, H256::zero(), 0..2);

    let headers = blocks.iter().map(|block| block.header.clone()).collect();
    let bodies = blocks
        .into_iter()
        .map(|block| {
            (
                block.hash(),
                BlockBody {
                    transactions: block.body,
                    ommers: block.ommers,
                    withdrawals: block.withdrawals,
                },
            )
        })
        .collect();

    (headers, bodies)
}

/// Generate a set of bodies, write them to a temporary file, and return the file along with the
/// bodies and corresponding block hashes
pub(crate) async fn generate_bodies_file(
    rng: RangeInclusive<u64>,
) -> (tokio::fs::File, Vec<SealedHeader>, HashMap<H256, BlockBody>) {
    let (headers, mut bodies) = generate_bodies(0..=19);
    let raw_block_bodies = create_raw_bodies(headers.clone().iter(), &mut bodies.clone());

    let mut file: File = tempfile::tempfile().unwrap().into();
    let mut writer = FramedWrite::new(file, BlockFileCodec);

    // rlp encode one after the other
    for block in raw_block_bodies {
        writer.send(block).await.unwrap();
    }

    // get the file back
    let mut file: File = writer.into_inner();
    file.seek(SeekFrom::Start(0)).await.unwrap();
    (file, headers, bodies)
}
