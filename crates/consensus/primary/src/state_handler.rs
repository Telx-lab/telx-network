// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) Telcoin, LLC
// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use consensus_metrics::spawn_logged_monitored_task;
use tn_types::{AuthorityIdentifier, Noticer, TaskManager, TnReceiver, TnSender};

use tap::TapFallible;
use tn_types::{Certificate, Round};
use tracing::{debug, error, info, warn};

use crate::ConsensusBus;

/// Updates Narwhal system state based on certificates received from consensus.
pub struct StateHandler {
    authority_id: AuthorityIdentifier,

    /// Used for Receives the ordered certificates from consensus.
    consensus_bus: ConsensusBus,
    /// Channel to signal committee changes.
    rx_shutdown: Noticer,

    network: anemo::Network,
}

impl StateHandler {
    pub fn spawn(
        authority_id: AuthorityIdentifier,
        consensus_bus: &ConsensusBus,
        rx_shutdown: Noticer,
        network: anemo::Network,
        task_manager: &TaskManager,
    ) {
        let state_handler =
            Self { authority_id, consensus_bus: consensus_bus.clone(), rx_shutdown, network };
        task_manager.spawn_task(
            "state handler task",
            spawn_logged_monitored_task!(
                async move {
                    state_handler.run().await;
                },
                "StateHandlerTask"
            ),
        );
    }

    async fn handle_sequenced(&mut self, commit_round: Round, certificates: Vec<Certificate>) {
        // Now we are going to signal which of our own batches have been committed.
        let own_rounds_committed: Vec<_> = certificates
            .iter()
            .filter_map(|cert| {
                if cert.header().author() == self.authority_id {
                    Some(cert.header().round())
                } else {
                    None
                }
            })
            .collect();
        debug!(target: "primary::state_handler", "Own committed rounds {:?} at round {:?}", own_rounds_committed, commit_round);

        // If a reporting channel is available send the committed own
        // headers to it.
        if let Err(e) = self
            .consensus_bus
            .committed_own_headers()
            .send((commit_round, own_rounds_committed))
            .await
        {
            error!(target: "primary::state_handler", "error sending commit header: {e}");
        }
    }

    async fn run(mut self) {
        info!(target: "primary::state_handler", "StateHandler on node {} has started successfully.", self.authority_id);
        // This clone inso a variable is D-U-M, subscribe should return an owned object but here we
        // are.
        let committed_certificates = self.consensus_bus.committed_certificates().clone();
        let mut rx_committed_certificates = committed_certificates.subscribe();
        loop {
            tokio::select! {
                Some((commit_round, certificates)) = rx_committed_certificates.recv() => {
                    self.handle_sequenced(commit_round, certificates).await;
                },

                _ = &self.rx_shutdown => {
                    // shutdown network
                    let _ = self.network.shutdown().await.tap_err(|err|{
                        error!(target: "primary::state_handler", "Error while shutting down network: {err}")
                    });

                    warn!(target: "primary::state_handler", "Network has shutdown");

                    return;
                }
            }
        }
    }
}
