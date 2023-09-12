// Copyright (c) Telcoin, LLC
// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use fastcrypto::hash::Hash;
use lattice_test_utils::CommitteeFixture;
use std::{vec, time::Duration};
use tn_network_types::{MockWorkerToWorker, WorkerToWorkerServer, WorkerSynchronizeMessage, RequestBatchesResponse, PrimaryToWorker, SealBatchRequest, EngineToWorker, SealedBatchResponse};
use lattice_typed_store::Map;
use tn_types::consensus::BatchAPI;
use super::*;
use crate::TrivialTransactionValidator;

#[tokio::test]
async fn test_engine_sends_batch() {
    telemetry_subscribers::init_for_testing();

    let fixture = CommitteeFixture::builder().randomize_ports(true).build();
    // let committee = fixture.committee();
    // let worker_cache = fixture.worker_cache();
    let authority = fixture.authorities().next().unwrap();
    let authority_id = authority.id();
    // let authority_key = authority.public_key();
    let id = 0;

    // Create a new test store.
    let store = lattice_test_utils::create_batch_store();

    let batch = lattice_test_utils::batch();
    let payload = batch.owned_transactions();
    let digest = batch.digest();
    let message = SealBatchRequest {
        payload: payload.clone()
    };
    let peer_id = authority.worker(id).peer_id();

    let (tx_quorum_waiter, mut rx_quorum_waiter) = lattice_test_utils::test_channel!(1);
    let handler = EngineToWorkerHandler {
        authority_id,
        id,
        peer_id,
        store: store.clone(),
        tx_quorum_waiter
    };

    // Verify the batch is not in store
    assert!(store.get(&digest).unwrap().is_none());
    
        // send quorum waiter ack
        tokio::spawn(async move {
            while let Some((_, _, reply)) = rx_quorum_waiter.recv().await {
                let _ = reply.send(());
            }
        });

    // Send a sync request.
    let request = anemo::Request::new(message);
    let expected = SealedBatchResponse {
        batch,
        digest,
        worker_id: id,
    };
    let response = handler.seal_batch(request).await.unwrap().into_body();

    // Verify it is now stored
    assert!(store.get(&digest).unwrap().is_some());
    assert_eq!(expected, response);
}

#[tokio::test]
async fn synchronize() {
    telemetry_subscribers::init_for_testing();

    let fixture = CommitteeFixture::builder().randomize_ports(true).build();
    let committee = fixture.committee();
    let worker_cache = fixture.worker_cache();
    let authority_id = fixture.authorities().next().unwrap().id();
    let id = 0;

    // Create a new test store.
    let store = lattice_test_utils::create_batch_store();

    // Create network with mock behavior to respond to RequestBatches request.
    let target_primary = fixture.authorities().nth(1).unwrap();
    let batch = lattice_test_utils::batch();
    let digest = batch.digest();
    let message = WorkerSynchronizeMessage {
        digests: vec![digest],
        target: target_primary.id(),
        is_certified: false,
    };

    let mut mock_server = MockWorkerToWorker::new();
    let mock_batch_response = batch.clone();
    mock_server
        .expect_request_batches()
        .withf(move |request| request.body().batch_digests == vec![digest])
        .return_once(move |_| {
            Ok(anemo::Response::new(RequestBatchesResponse {
                batches: vec![mock_batch_response],
                is_size_limit_reached: false,
            }))
        });
    let routes = anemo::Router::new().add_rpc_service(WorkerToWorkerServer::new(mock_server));
    let target_worker = target_primary.worker(id);
    let _recv_network = target_worker.new_network(routes);
    let send_network = lattice_test_utils::random_network();
    send_network
        .connect_with_peer_id(
            target_worker.info().worker_address.to_anemo_address().unwrap(),
            anemo::PeerId(target_worker.info().name.0.to_bytes()),
        )
        .await
        .unwrap();

    let handler = PrimaryToWorkerHandler {
        authority_id,
        id,
        committee,
        worker_cache,
        store: store.clone(),
        request_batch_timeout: Duration::from_secs(999),
        request_batch_retry_nodes: 3, // Not used in this test.
        network: Some(send_network),
        batch_fetcher: None,
        validator: TrivialTransactionValidator,
    };

    // Verify the batch is not in store
    assert!(store.get(&digest).unwrap().is_none());

    // Send a sync request.
    let request = anemo::Request::new(message);
    handler.synchronize(request).await.unwrap();

    // Verify it is now stored
    assert!(store.get(&digest).unwrap().is_some());
}

#[tokio::test]
async fn synchronize_when_batch_exists() {
    telemetry_subscribers::init_for_testing();

    let fixture = CommitteeFixture::builder().randomize_ports(true).build();
    let committee = fixture.committee();
    let worker_cache = fixture.worker_cache();
    let authority_id = fixture.authorities().next().unwrap().id();
    let id = 0;

    // Create a new test store.
    let store = lattice_test_utils::create_batch_store();

    // Create network without mock behavior since it will not be needed.
    let send_network = lattice_test_utils::random_network();

    let handler = PrimaryToWorkerHandler {
        authority_id,
        id,
        committee: committee.clone(),
        worker_cache,
        store: store.clone(),
        request_batch_timeout: Duration::from_secs(999),
        request_batch_retry_nodes: 3, // Not used in this test.
        network: Some(send_network),
        batch_fetcher: None,
        validator: TrivialTransactionValidator,
    };

    // Store the batch.
    let batch = lattice_test_utils::batch();
    let batch_id = batch.digest();
    let missing = vec![batch_id];
    store.insert(&batch_id, &batch).unwrap();

    // Send a sync request.
    let target_primary = fixture.authorities().nth(1).unwrap();
    let message = WorkerSynchronizeMessage {
        digests: missing.clone(),
        target: target_primary.id(),
        is_certified: false,
    };
    // The sync request should succeed.
    handler.synchronize(anemo::Request::new(message)).await.unwrap();
}
