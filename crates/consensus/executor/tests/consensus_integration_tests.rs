// Copyright (c) Telcoin, LLC
// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use fastcrypto::hash::Hash;
use narwhal_executor::{get_restored_consensus_output, MockExecutionState};
use narwhal_primary::{
    consensus::{
        Bullshark, Consensus, ConsensusMetrics, ConsensusRound, LeaderSchedule, LeaderSwapTable,
    },
    NUM_SHUTDOWN_RECEIVERS,
};
use narwhal_storage::NodeStorage;

use narwhal_types::test_utils::{temp_dir, CommitteeFixture};
use prometheus::Registry;

use std::{collections::BTreeSet, sync::Arc};
use tokio::sync::watch;

use narwhal_types::{Certificate, PreSubscribedBroadcastSender, Round};

#[tokio::test]
async fn test_recovery() {
    // Create storage
    let storage = NodeStorage::reopen(temp_dir(), None);

    let consensus_store = storage.consensus_store;
    let certificate_store = storage.certificate_store;

    // Setup consensus
    let fixture = CommitteeFixture::builder().build();
    let committee = fixture.committee();

    // Make certificates for rounds 1 up to 4.
    let ids: Vec<_> = fixture.authorities().map(|a| a.id()).collect();
    let genesis =
        Certificate::genesis(&committee).iter().map(|x| x.digest()).collect::<BTreeSet<_>>();
    let (mut certificates, next_parents) =
        narwhal_types::test_utils::make_optimal_certificates(&committee, 1..=4, &genesis, &ids);

    // Make two certificate (f+1) with round 5 to trigger the commits.
    let (_, certificate) =
        narwhal_types::test_utils::mock_certificate(&committee, ids[0], 5, next_parents.clone());
    certificates.push_back(certificate);
    let (_, certificate) =
        narwhal_types::test_utils::mock_certificate(&committee, ids[1], 5, next_parents);
    certificates.push_back(certificate);

    // Spawn the consensus engine and sink the primary channel.
    let (tx_waiter, rx_waiter) = narwhal_types::test_channel!(1);
    let (tx_primary, mut rx_primary) = narwhal_types::test_channel!(1);
    let (tx_output, mut rx_output) = narwhal_types::test_channel!(1);
    let (tx_consensus_round_updates, _rx_consensus_round_updates) =
        watch::channel(ConsensusRound::default());

    let mut tx_shutdown = PreSubscribedBroadcastSender::new(NUM_SHUTDOWN_RECEIVERS);

    const GC_DEPTH: Round = 50;
    const NUM_SUB_DAGS_PER_SCHEDULE: u64 = 100;
    let metrics = Arc::new(ConsensusMetrics::new(&Registry::new()));
    let bad_nodes_stake_threshold = 0;
    let bullshark = Bullshark::new(
        committee.clone(),
        consensus_store.clone(),
        metrics.clone(),
        NUM_SUB_DAGS_PER_SCHEDULE,
        LeaderSchedule::new(committee.clone(), LeaderSwapTable::default()),
        bad_nodes_stake_threshold,
    );

    let _consensus_handle = Consensus::spawn(
        committee,
        GC_DEPTH,
        consensus_store.clone(),
        certificate_store.clone(),
        tx_shutdown.subscribe(),
        rx_waiter,
        tx_primary,
        tx_consensus_round_updates,
        tx_output,
        bullshark,
        metrics,
    );
    tokio::spawn(async move { while rx_primary.recv().await.is_some() {} });

    // Feed all certificates to the consensus. Only the last certificate should trigger
    // commits, so the task should not block.
    while let Some(certificate) = certificates.pop_front() {
        // we store the certificates so we can enable the recovery
        // mechanism later.
        certificate_store.write(certificate.clone()).unwrap();
        tx_waiter.send(certificate).await.unwrap();
    }

    let expected_committed_sub_dags = 2;
    for i in 1..=expected_committed_sub_dags {
        let sub_dag = rx_output.recv().await.unwrap();
        assert_eq!(sub_dag.sub_dag_index, i);
    }

    // Now assume that we want to recover from a crash. We are testing all the recovery cases
    // from restoring the executed sub dag index = 0 up to 2.
    for last_executed_certificate_index in 0..=expected_committed_sub_dags {
        let mut execution_state = MockExecutionState::new();
        execution_state
            .expect_last_executed_sub_dag_index()
            .times(1)
            .returning(move || last_executed_certificate_index);

        let consensus_output = get_restored_consensus_output(
            consensus_store.clone(),
            certificate_store.clone(),
            &execution_state,
        )
        .await
        .unwrap();

        assert_eq!(consensus_output.len(), (2 - last_executed_certificate_index) as usize);
    }
}

// TODO:
// #[tokio::test]
// async fn test_internal_consensus_output() {
//     // Enabled debug tracing so we can easily observe the
//     // nodes logs.
//     let _guard = setup_tracing();

//     let mut cluster = Cluster::new(None);

//     // start the cluster
//     cluster.start(Some(4), Some(1), None).await;

//     // get a client to send transactions
//     let worker_id = 0;

//     let authority = cluster.authority(0);
//     // let mut client = authority.new_transactions_client(&worker_id).await;

//     // Subscribe to the transaction confirmation channel
//     let mut receiver = authority.primary().await.tx_transaction_confirmation.subscribe();

//     // Create arbitrary transactions
//     // let mut transactions = Vec::new();

//     const NUM_OF_TRANSACTIONS: u32 = 10;
//     // for i in 0..NUM_OF_TRANSACTIONS {
//     //     let tx = string_transaction(i);

//     //     // serialise and send
//     //     let tr = bcs::to_bytes(&tx).unwrap();
//     //     let txn = TransactionProto { transaction: Bytes::from(tr) };
//     //     client.submit_transaction(txn).await.unwrap();

//     //     transactions.push(tx);
//     // }

//     // wait for transactions to complete
//     loop {
//         let result = receiver.recv().await.unwrap();

//         // deserialise transaction
//         let output_transaction = bcs::from_bytes::<String>(&result).unwrap();

//         // we always remove the first transaction and check with the one
//         // sequenced. We want the transactions to be sequenced in the
//         // same order as we post them.
//         let expected_transaction = transactions.remove(0);

//         assert_eq!(
//             expected_transaction, output_transaction,
//             "Expected to have received transaction with same id. Ordering is important"
//         );

//         if transactions.is_empty() {
//             break;
//         }
//     }
// }

// fn string_transaction(id: u32) -> String {
//     format!("test transaction:{id}")
// }
