// Copyright(C) Facebook, Inc. and its affiliates.
// Copyright (c) Telcoin, LLC
// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use crate::{metrics::PrimaryMetrics, error::{PrimaryResult, PrimaryError}};
use indexmap::IndexMap;
use lattice_network::{PrimaryToEngineClient, client::NetworkClient};
use consensus_metrics::{
    metered_channel::{Receiver, Sender},
    spawn_logged_monitored_task,
};
use fastcrypto::hash::Hash as _;
use lattice_storage::ProposerStore;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, VecDeque, BTreeSet},
    sync::Arc,
};
use tn_types::consensus::{
    AuthorityIdentifier, Committee, Epoch, WorkerId,
    now, BatchDigest, Certificate, CertificateAPI, ConditionalBroadcastReceiver, Header, HeaderAPI,
    Round, TimestampSec,
};
use tn_network_types::{
    BuildHeaderRequest, HeaderPayloadResponse,
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
    time::{sleep, sleep_until, Duration, Instant},
};
use tracing::{debug, enabled, error, info, trace};

/// Messages sent to the proposer about our own batch digests
#[derive(Debug)]
pub struct OurDigestMessage {
    pub digest: BatchDigest,
    pub worker_id: WorkerId,
    pub timestamp: TimestampSec,
    /// A channel to send an () as an ack after this digest is processed by the primary.
    pub ack_channel: Option<oneshot::Sender<()>>,
}

const DEFAULT_HEADER_RESEND_TIMEOUT: Duration = Duration::from_secs(60);

/// The proposer creates new headers and send them to the core for broadcasting and further
/// processing.
pub struct Proposer {
    /// The id of this primary.
    authority_id: AuthorityIdentifier,
    /// The committee information.
    committee: Committee,
    /// The threshold number of batches that can trigger
    /// a header creation. When there are available at least
    /// `header_num_of_batches_threshold` batches we are ok
    /// to try and propose a header
    header_num_of_batches_threshold: usize,
    /// The maximum number of batches in header.
    max_header_num_of_batches: usize,
    /// The maximum delay to wait for conditions like having leader in parents.
    max_header_delay: Duration,
    /// The minimum delay between generating headers.
    min_header_delay: Duration,
    /// The delay to wait until resending the last proposed header if proposer
    /// hasn't proposed anything new since then. If None is provided then the
    /// default value will be used instead.
    header_resend_timeout: Option<Duration>,
    /// Receiver for shutdown.
    rx_shutdown: ConditionalBroadcastReceiver,
    /// Receives the parents to include in the next header (along with their round number) from
    /// core.
    rx_parents: Receiver<(Vec<Certificate>, Round, Epoch)>,
    /// Receives the batches' digests from our workers.
    rx_our_digests: Receiver<OurDigestMessage>,
    /// Sends newly created headers to the `Certifier`.
    tx_headers: Sender<Header>,
    /// The proposer store for persisting the last header.
    proposer_store: ProposerStore,
    /// The current round of the dag.
    round: Round,
    /// Last time the round has been updated
    last_round_timestamp: Option<TimestampSec>,
    /// Signals a new narwhal round
    tx_narwhal_round_updates: watch::Sender<Round>,
    /// Holds the certificates' ids waiting to be included in the next header.
    last_parents: Vec<Certificate>,
    /// Holds the certificate of the last leader (if any).
    last_leader: Option<Certificate>,
    /// Holds the batches' digests waiting to be included in the next header.
    /// Digests are roughly oldest to newest, and popped in FIFO order from the front.
    digests: VecDeque<OurDigestMessage>,
    /// Holds the map of proposed previous round headers and their digest messages, to ensure that
    /// all batches' digest included will eventually be re-sent.
    proposed_headers: BTreeMap<Round, (Header, VecDeque<OurDigestMessage>)>,
    /// Committed headers channel on which we get updates on which of
    /// our own headers have been committed.
    rx_committed_own_headers: Receiver<(Round, Vec<Round>)>,
    /// Metrics handler
    metrics: Arc<PrimaryMetrics>,
    /// Network for sending rpc call to EL engine.
    network_client: NetworkClient,
}

impl Proposer {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn spawn(
        authority_id: AuthorityIdentifier,
        committee: Committee,
        proposer_store: ProposerStore,
        header_num_of_batches_threshold: usize,
        max_header_num_of_batches: usize,
        max_header_delay: Duration,
        min_header_delay: Duration,
        header_resend_timeout: Option<Duration>,
        rx_shutdown: ConditionalBroadcastReceiver,
        rx_parents: Receiver<(Vec<Certificate>, Round, Epoch)>,
        rx_our_digests: Receiver<OurDigestMessage>,
        tx_headers: Sender<Header>,
        tx_narwhal_round_updates: watch::Sender<Round>,
        rx_committed_own_headers: Receiver<(Round, Vec<Round>)>,
        metrics: Arc<PrimaryMetrics>,
        network_client: NetworkClient,
    ) -> JoinHandle<()> {
        let genesis = Certificate::genesis(&committee);
        spawn_logged_monitored_task!(
            async move {
                Self {
                    authority_id,
                    committee,
                    header_num_of_batches_threshold,
                    max_header_num_of_batches,
                    max_header_delay,
                    min_header_delay,
                    header_resend_timeout,
                    rx_shutdown,
                    rx_parents,
                    rx_our_digests,
                    tx_headers,
                    tx_narwhal_round_updates,
                    proposer_store,
                    round: 0,
                    last_round_timestamp: None,
                    last_parents: genesis,
                    last_leader: None,
                    digests: VecDeque::with_capacity(2 * max_header_num_of_batches),
                    proposed_headers: BTreeMap::new(),
                    rx_committed_own_headers,
                    metrics,
                    network_client,
                }
                .run()
                .await;
            },
            "ProposerTask"
        )
    }

    /// Spawn a network task to request the EL to build the next block for propsal.
    /// After receiving data from the EL, this method completes construction of
    /// the block, but does not store or broadcast it.
    /// 
    /// The task returns a oneshot channel receiver that receives the built header
    /// or an error upon completion. An Option<VecDeque<OurDigestMessage>> is returned
    /// if the header is new for this round.
    async fn spawn_build_header(&mut self) -> oneshot::Receiver<PrimaryResult<(Header, Option<VecDeque<OurDigestMessage>>)>> {
        let this_round = self.round;
        let this_epoch = self.committee.epoch();

        // result channel
        let (tx, rx) = oneshot::channel();

        // Check if we already have stored a header for this round.
        match self.proposer_store.get_last_proposed() {
            Ok(Some(last_header)) => {
                // clear last parents if the header is from this round
                if last_header.round() == this_round && last_header.epoch() == this_epoch {
                    // We have already produced a header for the current round, idempotent re-send
                    debug!("Proposer re-using existing header for round {this_round}");
                    self.last_parents.clear(); // Clear parents that are now invalid for next round.
                    let _ = tx.send(Ok((last_header, None)));
                    return rx
                }
            }
            Ok(None) => (),
            Err(e) => {
                let _ = tx.send(Err(e.into()));
                return rx
            },
        }

        // Make a new header:
        //
        // these values could change while waiting for the network response from EL
        // so we drain the current digests and last_parents
        let num_of_digests = self.digests.len().min(self.max_header_num_of_batches);
        let parent_certs: Vec<_> = self.last_parents.drain(..).collect();
        let header_digests: VecDeque<_> = self.digests.drain(..num_of_digests).collect();
        let payload: IndexMap<BatchDigest, (u32, u64)> =
            header_digests.iter().map(|m| (m.digest, (m.worker_id, m.timestamp))).collect();

        // Here we check that the timestamp we will include in the header is consistent with the
        // parents, ie our current time is *after* the timestamp in all the included headers. If
        // not we log an error and hope a kind operator fixes the clock.
        let parent_max_time = parent_certs.iter().map(|c| *c.header().created_at()).max().unwrap_or(0);
        let current_time = now();
        if current_time < parent_max_time {
            let drift_ms = parent_max_time - current_time;
            error!(
                "Current time {} earlier than max parent time {}, sleeping for {}ms until max parent time.",
                current_time, parent_max_time, drift_ms,
            );
            self.metrics.header_max_parent_wait_ms.inc_by(drift_ms);
            sleep(Duration::from_millis(drift_ms)).await;
        }

        // calculate for metrics
        let leader_and_support = if this_round % 2 == 0 {
            let authority = self.committee.leader(this_round);
            if self.authority_id == authority.id() {
                "even_round_is_leader"
            } else {
                "even_round_not_leader"
            }
        } else {
            let authority = self.committee.leader(this_round - 1);
            if parent_certs.iter().any(|c| c.origin() == authority.id()) {
                "odd_round_gives_support"
            } else {
                "odd_round_no_support"
            }
        };

        let parents: BTreeSet<_> = self.last_parents.iter().map(|cert| cert.digest()).collect();
        let network_client = self.network_client.clone();
        let authority_id = self.authority_id;
        let metrics = self.metrics.clone();
        let max_header_delay = self.max_header_delay;

        // spawn network task
        tokio::spawn(async move {
            // EL data
            let request = BuildHeaderRequest {
                round: this_round,
                epoch: this_epoch,
                created_at: current_time,
                payload: payload.clone(),
                parents: parents.clone(),
            };

            match network_client.build_header(request).await {
                Ok(HeaderPayloadResponse { sealed_header }) => {
                    let header = Header::new(
                        authority_id,
                        this_round,
                        this_epoch,
                        current_time,
                        payload,
                        parents,
                        sealed_header,
                    );

                    // update metrics
                    Proposer::update_metrics(metrics, max_header_delay, leader_and_support, &header, &header_digests);

                    tx.send(Ok((header, Some(header_digests))))
                }
                Err(e) => tx.send(Err(e.into())),
            }
        });

        rx
    }

    /// Process a header for the round and persist it to database.
    /// 
    /// This method takes a built header produced by the EL and
    /// sends it to core for processing. If successful, it returns
    /// the number of batch digests included in header.
    /// 
    /// Note: only new headers inlcude the VecDeque<OurDigestMessage> for
    /// inserting to self.proposed_headers. If the header was already proposed
    /// for this round, the option is `None`.
    async fn process_header(
        &mut self,
        header: Header,
        mut digests: Option<VecDeque<OurDigestMessage>>,
    ) -> PrimaryResult<(Header, usize)> {
        // Some() means proposer created a new header
        if let Some(digests) = digests.take() {
            // Register the header by the current round, to remember that we need to commit
            // it, or re-include the batch digests that it contains.
            self.proposed_headers.insert(self.round, (header.clone(), digests));
        }

        // Store the last header.
        self.proposer_store.write_last_proposed(&header)?;

        #[cfg(feature = "benchmark")]
        for digest in header.payload().keys() {
            // NOTE: This log entry is used to compute performance.
            info!("Created {} -> {:?}", header, digest);
        }

        let num_of_included_digests = header.payload().len();

        // Send the new header to the `Certifier` that will broadcast and certify it.
        self.tx_headers.send(header.clone()).await.map_err(|_| PrimaryError::ShuttingDown)?;

        Ok((header, num_of_included_digests))
    }

    /// Update metrics for newly built header.
    /// 
    /// The method ensures we are protected against equivocation.
    /// If we detect that a different header has already been produced for the same round, then
    /// this method returns the earlier header. Otherwise the newly created header will be returned.
    fn update_metrics(
        metrics: Arc<PrimaryMetrics>,
        max_header_delay: Duration,
        leader_and_support: &str,
        header: &Header,
        digests: &VecDeque<OurDigestMessage>,
    ) {
        let parents = header.parents();
        metrics.headers_proposed.with_label_values(&[leader_and_support]).inc();
        metrics.header_parents.observe(parents.len() as f64);

        if enabled!(tracing::Level::TRACE) {
            let mut msg = format!("Created header {header:?} with parent certificates:\n");
            for parent in parents.iter() {
                msg.push_str(&format!("{parent:?}\n"));
            }
            trace!(msg);
        } else {
            debug!("Created header {header:?}");
        }

        // Update metrics related to latency
        let mut total_inclusion_secs = 0.0;
        for digest in digests {
            let batch_inclusion_secs =
                Duration::from_millis(*header.created_at() - digest.timestamp).as_secs_f64();
            total_inclusion_secs += batch_inclusion_secs;

            // NOTE: This log entry is used to compute performance.
            tracing::debug!(
                    "Batch {:?} from worker {} took {} seconds from creation to be included in a proposed header",
                    digest.digest,
                    digest.worker_id,
                    batch_inclusion_secs
                );
            metrics.proposer_batch_latency.observe(batch_inclusion_secs);
        }

        // NOTE: This log entry is used to compute performance.
        let (header_creation_secs, avg_inclusion_secs) =
            if let Some(digest) = digests.front() {
                (
                    Duration::from_millis(*header.created_at() - digest.timestamp).as_secs_f64(),
                    total_inclusion_secs / digests.len() as f64,
                )
            } else {
                (max_header_delay.as_secs_f64(), 0.0)
            };
        debug!(
            "Header {:?} was created in {} seconds. Contains {} batches, with average delay {} seconds.",
            header.digest(),
            header_creation_secs,
            digests.len(),
            avg_inclusion_secs,
        );
    }

    fn max_delay(&self) -> Duration {
        // If this node is going to be the leader of the next round, we set a lower max
        // timeout value to increase its chance of being included in the dag.
        if self.committee.leader(self.round + 1).id() == self.authority_id {
            self.max_header_delay / 2
        } else {
            self.max_header_delay
        }
    }

    fn min_delay(&self) -> Duration {
        // If this node is going to be the leader of the next round and there are more than
        // 1 primary in the committee, we use a lower min delay value to increase the chance
        // of committing the leader.
        if self.committee.size() > 1 &&
            self.committee.leader(self.round + 1).id() == self.authority_id
        {
            Duration::ZERO
        } else {
            self.min_header_delay
        }
    }

    /// Update the last leader certificate.
    fn update_leader(&mut self) -> bool {
        let leader = self.committee.leader(self.round);
        self.last_leader = self
            .last_parents
            .iter()
            .find(|x| {
                if x.origin() == leader.id() {
                    debug!("Got leader {:?} for round {}", x, self.round);
                    true
                } else {
                    false
                }
            })
            .cloned();

        self.last_leader.is_some()
    }

    /// Check whether if this validator is the leader of the round, or if we have
    /// (i) f+1 votes for the leader, (ii) 2f+1 nodes not voting for the leader,
    /// (iii) there is no leader to vote for.
    fn enough_votes(&self) -> bool {
        if self.committee.leader(self.round + 1).id() == self.authority_id {
            return true
        }

        let leader = match &self.last_leader {
            Some(x) => x.digest(),
            None => return true,
        };

        let mut votes_for_leader = 0;
        let mut no_votes = 0;
        for certificate in &self.last_parents {
            let stake = self.committee.stake_by_id(certificate.origin());
            if certificate.header().parents().contains(&leader) {
                votes_for_leader += stake;
            } else {
                no_votes += stake;
            }
        }

        let mut enough_votes = votes_for_leader >= self.committee.validity_threshold();
        enough_votes |= no_votes >= self.committee.quorum_threshold();
        enough_votes
    }

    /// Whether we can advance the DAG or need to wait for the leader/more votes.
    /// Note that if we timeout, we ignore this check and advance anyway.
    fn ready(&mut self) -> bool {
        match self.round % 2 {
            0 => self.update_leader(),
            _ => self.enough_votes(),
        }
    }

    /// Main loop listening to incoming messages.
    pub async fn run(&mut self) {
        debug!("Dag starting at round {}", self.round);
        let mut advance = true;

        let timer_start = Instant::now();
        let max_delay_timer = sleep_until(timer_start + self.max_header_delay);
        let min_delay_timer = sleep_until(timer_start + self.min_header_delay);

        let header_resend_timeout =
            self.header_resend_timeout.unwrap_or(DEFAULT_HEADER_RESEND_TIMEOUT);
        let mut header_repeat_timer = Box::pin(sleep(header_resend_timeout));
        let mut opt_latest_header = None;

        tokio::pin!(max_delay_timer);
        tokio::pin!(min_delay_timer);

        info!(
            "Proposer on node {} has started successfully with header resend timeout {:?}.",
            self.authority_id, header_resend_timeout
        );
        loop {
            // Check if we can propose a new header. We propose a new header when we have a quorum
            // of parents and one of the following conditions is met:
            // (i) the timer expired
            //   - we timed out on the leader or gave up gather votes for the
            //     leader,
            // (ii) we have enough digests
            //   - header_num_of_batches_threshold
            // and we are on the happy path (we can vote for the leader or the leader
            // has enough votes to enable a commit).
            //
            // We guarantee that no more than
            // max_header_num_of_batches are included.
            let enough_parents = !self.last_parents.is_empty();
            let enough_digests = self.digests.len() >= self.header_num_of_batches_threshold;
            let max_delay_timed_out = max_delay_timer.is_elapsed();
            let min_delay_timed_out = min_delay_timer.is_elapsed();

            // optional channel if the primary can and should build a new header
            let opt_channel = if (max_delay_timed_out || ((enough_digests || min_delay_timed_out) && advance)) &&
                enough_parents
            {
                if max_delay_timed_out {
                    // It is expected that this timer expires from time to time. If it expires too
                    // often, it either means some validators are Byzantine or
                    // that the network is experiencing periods of asynchrony.
                    // In practice, the latter scenario means we misconfigured the parameter
                    // called `max_header_delay`.
                    debug!("Timer expired for round {}", self.round);
                }

                // Advance to the next round.
                self.round += 1;
                let _ = self.tx_narwhal_round_updates.send(self.round);

                // Update the metrics
                self.metrics.current_round.set(self.round as i64);

                let current_timestamp = now();
                let reason = if max_delay_timed_out {
                    "max_timeout"
                } else if enough_digests {
                    "threshold_size_reached"
                } else {
                    "min_timeout"
                };

                if let Some(t) = &self.last_round_timestamp {
                    self.metrics
                        .proposal_latency
                        .with_label_values(&[reason])
                        .observe(Duration::from_millis(current_timestamp - t).as_secs_f64());
                }
                self.last_round_timestamp = Some(current_timestamp);

                debug!("Dag moved to round {}", self.round);
                
                // build the next header and return the receiver channel
                let rx = self.spawn_build_header().await;

                Some(rx)
            } else { None };

            // workaround for tokio::select!
            let next_header = async move {
                match opt_channel {
                    Some(rx) => {
                        let res = rx.await.map_err(|_| PrimaryError::ClosedChannel("next header".to_string()))?;
                        match res {
                            Err(e @ PrimaryError::ShuttingDown) => {
                                debug!("{e}");
                                Err(e)
                            }
                            Err(e) => {
                                panic!("Unexpected error: {e}");
                            }
                            Ok(res) => Ok(res),
                        }
                    },
                    None => Err(PrimaryError::ClosedChannel("next header".to_string())),
                }
            };

            tokio::select! {
                Ok((header, digests)) = next_header => {
                    match self.process_header(header, digests).await {
                        Err(e @ PrimaryError::ShuttingDown) => debug!("{e}"),
                        Err(e) => panic!("Unexpected error: {e}"),
                        Ok((header, digests)) => {
                            let reason = if max_delay_timed_out {
                                "max_timeout"
                            } else if enough_digests {
                                "threshold_size_reached"
                            } else {
                                "min_timeout"
                            };
                            // Save the header
                            opt_latest_header = Some(header);
                            header_repeat_timer = Box::pin(sleep(header_resend_timeout));

                            self.metrics
                                .num_of_batch_digests_in_header
                                .with_label_values(&[reason])
                                .observe(digests as f64);
                        }
                    }
                    // Reschedule the timer.
                    let timer_start = Instant::now();
                    max_delay_timer.as_mut().reset(timer_start + self.max_delay());
                    min_delay_timer.as_mut().reset(timer_start + self.min_delay());
                }

                () = &mut header_repeat_timer => {
                    // If the round has not advanced within header_resend_timeout then try to
                    // re-process our own header.
                    if let Some(header) = &opt_latest_header {
                        debug!("resend header {:?}", header);

                        if let Err(err) = self.tx_headers.send(header.clone()).await.map_err(|_| PrimaryError::ShuttingDown) {
                            error!("failed to resend header {:?} : {:?}", header, err);
                        }

                        // we want to reset the timer only when there is already a previous header
                        // created.
                        header_repeat_timer = Box::pin(sleep(header_resend_timeout));
                    }
                }

                Some((commit_round, commit_headers)) = self.rx_committed_own_headers.recv() => {
                    // Remove committed headers from the list of pending
                    let mut max_committed_round = 0;
                    for round in commit_headers {
                        max_committed_round = max_committed_round.max(round);
                        let Some(_) = self.proposed_headers.remove(&round) else {
                            info!("Own committed header not found at round {round}, probably because of restarts.");
                            // There can still be later committed headers in proposed_headers.
                            continue;
                        };
                    }

                    // Now for any round below the current commit round we re-insert
                    // the batches into the digests we need to send, effectively re-sending
                    // them in FIFO order.
                    // Oldest to newest payloads.
                    let mut digests_to_resend = VecDeque::new();
                    // Oldest to newest rounds.
                    let mut retransmit_rounds = Vec::new();

                    // Iterate in order of rounds of our own headers.
                    for (header_round, (_header, included_digests)) in &mut self.proposed_headers {
                        // Stop once we have processed headers at and below last committed round.
                        if *header_round > max_committed_round {
                            break;
                        }
                        // Add payloads from oldest to newest.
                        digests_to_resend.append(included_digests);
                        retransmit_rounds.push(*header_round);
                    }

                    if !retransmit_rounds.is_empty() {
                        let num_to_resend = digests_to_resend.len();
                        // Since all of digests_to_resend are roughly newer than self.digests,
                        // prepend digests_to_resend to the digests for the next header.
                        digests_to_resend.append(&mut self.digests);
                        self.digests = digests_to_resend;

                        // Now delete the headers with batches we re-transmit
                        for round in &retransmit_rounds {
                            self.proposed_headers.remove(round);
                        }

                        debug!(
                            "Retransmit {} batches in undelivered headers {:?} at commit round {:?}, remaining headers {}",
                            num_to_resend,
                            retransmit_rounds,
                            commit_round,
                            self.proposed_headers.len()
                        );

                        self.metrics.proposer_resend_headers.inc_by(retransmit_rounds.len() as u64);
                        self.metrics.proposer_resend_batches.inc_by(num_to_resend as u64);
                    }
                },

                Some((parents, round, epoch)) = self.rx_parents.recv() => {
                    // If the core already moved to the next epoch we should pull the next
                    // committee as well.

                    match epoch.cmp(&self.committee.epoch()) {
                        Ordering::Equal => {
                            // we can proceed.
                        }
                        _ => continue
                    }

                    // Sanity check: verify provided certs are of the correct round & epoch.
                    for parent in parents.iter() {
                        if parent.round() != round || parent.epoch() != epoch {
                            error!("Proposer received certificate {parent:?} that failed to match expected round {round} or epoch {epoch}. This should not be possible.");
                        }
                    }

                    // Compare the parents' round number with our current round.
                    match round.cmp(&self.round) {
                        Ordering::Greater => {
                            // We accept round bigger than our current round to jump ahead in case we were
                            // late (or just joined the network).
                            self.round = round;
                            let _ = self.tx_narwhal_round_updates.send(self.round);
                            self.last_parents = parents;

                            // we re-calculate the timeout to give the opportunity to the node
                            // to propose earlier if it's a leader for the round
                            // Reschedule the timer.
                            let timer_start = Instant::now();
                            max_delay_timer
                                .as_mut()
                                .reset(timer_start + self.max_delay());
                            min_delay_timer
                                .as_mut()
                                .reset(timer_start + self.min_delay());
                        },
                        Ordering::Less => {
                            // Ignore parents from older rounds.
                            continue;
                        },
                        Ordering::Equal => {
                            // The core gives us the parents the first time they are enough to form a quorum.
                            // Then it keeps giving us all the extra parents.
                            self.last_parents.extend(parents)
                        }
                    }

                    // Check whether we can advance to the next round. Note that if we timeout,
                    // we ignore this check and advance anyway.
                    advance = if self.ready() {
                        if !advance {
                            debug!(
                                "Ready to advance from round {}",
                                self.round,
                            );
                        }
                        true
                    } else {
                        false
                    };

                    let round_type = if self.round % 2 == 0 {
                        "even"
                    } else {
                        "odd"
                    };

                    self.metrics
                    .proposer_ready_to_advance
                    .with_label_values(&[&advance.to_string(), round_type])
                    .inc();
                }

                // Receive digests from our workers.
                Some(mut message) = self.rx_our_digests.recv() => {
                    // Signal back to the worker that the batch is recorded on the
                    // primary, and will be tracked until inclusion. This means that
                    // if the primary does not fail it will attempt to send the digest
                    // (and re-send if necessary) until it is sequenced, or the end of
                    // the epoch is reached. For the moment this does not persist primary
                    // crashes and re-starts.
                    let _ = message.ack_channel.take().unwrap().send(());
                    self.digests.push_back(message);
                }

                // Check whether any timer expired.
                () = &mut max_delay_timer, if !max_delay_timed_out => {
                    // Continue to next iteration of the loop.
                }
                () = &mut min_delay_timer, if !min_delay_timed_out => {
                    // Continue to next iteration of the loop.
                }

                _ = self.rx_shutdown.receiver.recv() => {
                    return
                }
            }

            // update metrics
            self.metrics.num_of_pending_batches_in_proposer.set(self.digests.len() as i64);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::NUM_SHUTDOWN_RECEIVERS;
    use fastcrypto::traits::KeyPair;
    use indexmap::IndexMap;
    use lattice_test_utils::{fixture_payload, CommitteeFixture, payload_builder, setup_tracing};
    use prometheus::Registry;
    use tn_tracing::init_test_tracing;
    use tn_types::consensus::PreSubscribedBroadcastSender;
    use tn_network_types::{MockPrimaryToEngine, HeaderPayloadResponse};

    // TODO: test execute_header()
    // - sealed_header and digest updated
    // - should header verify checks sealed header field?

    #[tokio::test]
    async fn propose_empty() {
        init_test_tracing();
        let fixture = CommitteeFixture::builder().build();
        let committee = fixture.committee();
        let worker_cache = fixture.worker_cache();
        let primary = fixture.authorities().next().unwrap();
        let name = primary.id();

        let mut tx_shutdown = PreSubscribedBroadcastSender::new(NUM_SHUTDOWN_RECEIVERS);
        let (_tx_parents, rx_parents) = lattice_test_utils::test_channel!(1);
        let (_tx_committed_own_headers, rx_committed_own_headers) =
            lattice_test_utils::test_channel!(1);
        let (_tx_our_digests, rx_our_digests) = lattice_test_utils::test_channel!(1);
        let (tx_headers, mut rx_headers) = lattice_test_utils::test_channel!(1);
        let (tx_narwhal_round_updates, _rx_narwhal_round_updates) = watch::channel(0u64);

        let metrics = Arc::new(PrimaryMetrics::new(&Registry::new()));
        let client = NetworkClient::new_from_keypair(&primary.network_keypair(), &primary.engine_network_keypair().public());

        // mock engine header provider
        let mut mock_engine = MockPrimaryToEngine::new();
        mock_engine.expect_build_header().return_once(
            move |_request| {
                tracing::debug!("mock engine expect_build_header: {_request:?}");
                let header = tn_types::execution::Header::default();
                Ok(
                    anemo::Response::new(
                        HeaderPayloadResponse {
                            sealed_header: header.seal_slow(),
                        }
                    )
                )
            }
        );

        client.set_primary_to_engine_local_handler(Arc::new(mock_engine));

        // Spawn the proposer.
        let _proposer_handle = Proposer::spawn(
            name,
            committee.clone(),
            ProposerStore::new_for_tests(),
            /* header_num_of_batches_threshold */ 32,
            /* max_header_num_of_batches */ 100,
            /* max_header_delay */ Duration::from_millis(20),
            /* min_header_delay */ Duration::from_millis(20),
            None,
            tx_shutdown.subscribe(),
            /* rx_core */ rx_parents,
            /* rx_workers */ rx_our_digests,
            /* tx_core */ tx_headers,
            tx_narwhal_round_updates,
            rx_committed_own_headers,
            metrics,
            client,
        );

        // Ensure the proposer makes a correct empty header.
        let header = rx_headers.recv().await.unwrap();
        assert_eq!(header.round(), 1);
        assert!(header.payload().is_empty());
        assert!(header.validate(&committee, &worker_cache).is_ok());
    }

    #[tokio::test]
    async fn propose_payload_and_repropose_after_n_seconds() {
        let fixture = CommitteeFixture::builder().build();
        let committee = fixture.committee();
        let worker_cache = fixture.worker_cache();
        let primary = fixture.authorities().next().unwrap();
        let name = primary.id();
        let header_resend_delay = Duration::from_secs(3);

        let mut tx_shutdown = PreSubscribedBroadcastSender::new(NUM_SHUTDOWN_RECEIVERS);
        let (tx_parents, rx_parents) = lattice_test_utils::test_channel!(1);
        let (tx_our_digests, rx_our_digests) = lattice_test_utils::test_channel!(1);
        let (_tx_committed_own_headers, rx_committed_own_headers) =
            lattice_test_utils::test_channel!(1);
        let (tx_headers, mut rx_headers) = lattice_test_utils::test_channel!(1);
        let (tx_narwhal_round_updates, _rx_narwhal_round_updates) = watch::channel(0u64);

        let metrics = Arc::new(PrimaryMetrics::new(&Registry::new()));

        let max_num_of_batches = 10;
        let client = NetworkClient::new_from_keypair(&primary.network_keypair(), &primary.engine_network_keypair().public());

        // mock engine header provider
        let mut mock_engine = MockPrimaryToEngine::new();
        mock_engine.expect_build_header().times(2).returning(
            move |_request| {
                tracing::debug!("mock engine expect_build_header: {_request:?}");
                let header = tn_types::execution::Header::default();
                Ok(
                    anemo::Response::new(
                        HeaderPayloadResponse {
                            sealed_header: header.seal_slow(),
                        }
                    )
                )
            }
        );

        client.set_primary_to_engine_local_handler(Arc::new(mock_engine));

        // Spawn the proposer.
        let _proposer_handle = Proposer::spawn(
            name,
            committee.clone(),
            ProposerStore::new_for_tests(),
            /* header_num_of_batches_threshold */ 1,
            /* max_header_num_of_batches */ max_num_of_batches,
            /* max_header_delay */
            Duration::from_millis(1_000_000), // Ensure it is not triggered.
            /* min_header_delay */
            Duration::from_millis(1_000_000), // Ensure it is not triggered.
            Some(header_resend_delay),
            tx_shutdown.subscribe(),
            /* rx_core */ rx_parents,
            /* rx_workers */ rx_our_digests,
            /* tx_core */ tx_headers,
            tx_narwhal_round_updates,
            rx_committed_own_headers,
            metrics,
            client,
        );

        // Send enough digests for the header payload.
        let mut b = [0u8; 32];
        let r: Vec<u8> = (0..32).map(|_v| rand::random::<u8>()).collect();
        b.copy_from_slice(r.as_slice());

        let digest = BatchDigest(b);
        let worker_id = 0;
        let created_at_ts = 0;
        let (tx_ack, rx_ack) = tokio::sync::oneshot::channel();
        tx_our_digests
            .send(OurDigestMessage {
                digest,
                worker_id,
                timestamp: created_at_ts,
                ack_channel: Some(tx_ack),
            })
            .await
            .unwrap();

        // Ensure the proposer makes a correct header from the provided payload.
        let header = rx_headers.recv().await.unwrap();
        assert_eq!(header.round(), 1);
        assert_eq!(header.payload().get(&digest), Some(&(worker_id, created_at_ts)));
        assert!(header.validate(&committee, &worker_cache).is_ok());

        // WHEN available batches are more than the maximum ones
        let batches: IndexMap<BatchDigest, (WorkerId, TimestampSec)> =
            fixture_payload((max_num_of_batches * 2) as u8);

        let mut ack_list = vec![];
        for (batch_id, (worker_id, created_at)) in batches {
            let (tx_ack, rx_ack) = tokio::sync::oneshot::channel();
            tx_our_digests
                .send(OurDigestMessage {
                    digest: batch_id,
                    worker_id,
                    timestamp: created_at,
                    ack_channel: Some(tx_ack),
                })
                .await
                .unwrap();

            ack_list.push(rx_ack);

            tokio::task::yield_now().await;
        }

        // AND send some parents to advance the round
        let parents: Vec<_> =
            fixture.headers().iter().take(4).map(|h| fixture.certificate(h)).collect();

        let result = tx_parents.send((parents, 1, 0)).await;
        assert!(result.is_ok());

        // THEN the header should contain max_num_of_batches
        let header = rx_headers.recv().await.unwrap();
        assert_eq!(header.round(), 2);
        assert_eq!(header.payload().len(), max_num_of_batches);
        assert!(rx_ack.await.is_ok());

        // Check all batches are acked.
        for rx_ack in ack_list {
            assert!(rx_ack.await.is_ok());
        }

        // WHEN wait to fetch again from the rx_headers a few times.
        // In theory after header_resend_delay we should receive again
        // the last created header.
        for _ in 0..3 {
            let resent_header = rx_headers.recv().await.unwrap();

            // THEN should be the exact same as the last sent
            assert_eq!(header, resent_header);
        }
    }

    #[tokio::test]
    async fn equivocation_protection() {
        let fixture = CommitteeFixture::builder().build();
        let committee = fixture.committee();
        let worker_cache = fixture.worker_cache();
        let primary = fixture.authorities().next().unwrap();
        let authority_id = primary.id();
        let proposer_store = ProposerStore::new_for_tests();

        let mut tx_shutdown = PreSubscribedBroadcastSender::new(NUM_SHUTDOWN_RECEIVERS);
        let (tx_parents, rx_parents) = lattice_test_utils::test_channel!(1);
        let (tx_our_digests, rx_our_digests) = lattice_test_utils::test_channel!(1);
        let (tx_headers, mut rx_headers) = lattice_test_utils::test_channel!(1);
        let (tx_narwhal_round_updates, _rx_narwhal_round_updates) = watch::channel(0u64);
        let (_tx_committed_own_headers, rx_committed_own_headers) =
            lattice_test_utils::test_channel!(1);
        let metrics = Arc::new(PrimaryMetrics::new(&Registry::new()));
        let client = NetworkClient::new_from_keypair(&primary.network_keypair(), &primary.engine_network_keypair().public());

        // mock engine header provider
        let mut mock_engine = MockPrimaryToEngine::new();
        mock_engine.expect_build_header().return_once(
            move |_request| {
                tracing::debug!("mock engine expect_build_header: {_request:?}");
                let header = tn_types::execution::Header::default();
                Ok(
                    anemo::Response::new(
                        HeaderPayloadResponse {
                            sealed_header: header.seal_slow(),
                        }
                    )
                )
            }
        );

        client.set_primary_to_engine_local_handler(Arc::new(mock_engine));

        // Spawn the proposer.
        let proposer_handle = Proposer::spawn(
            authority_id,
            committee.clone(),
            proposer_store.clone(),
            /* header_num_of_batches_threshold */ 1,
            /* max_header_num_of_batches */ 10,
            /* max_header_delay */
            Duration::from_millis(1_000_000), // Ensure it is not triggered.
            /* min_header_delay */
            Duration::from_millis(1_000_000), // Ensure it is not triggered.
            None,
            tx_shutdown.subscribe(),
            /* rx_core */ rx_parents,
            /* rx_workers */ rx_our_digests,
            /* tx_core */ tx_headers,
            tx_narwhal_round_updates,
            rx_committed_own_headers,
            metrics,
            client,
        );

        // Send enough digests for the header payload.
        let mut b = [0u8; 32];
        let r: Vec<u8> = (0..32).map(|_v| rand::random::<u8>()).collect();
        b.copy_from_slice(r.as_slice());

        let digest = BatchDigest(b);
        let worker_id = 0;
        let created_at_ts = 0;
        let (tx_ack, rx_ack) = tokio::sync::oneshot::channel();
        tx_our_digests
            .send(OurDigestMessage {
                digest,
                worker_id,
                timestamp: created_at_ts,
                ack_channel: Some(tx_ack),
            })
            .await
            .unwrap();

        // Create and send parents
        let parents: Vec<_> =
            fixture.headers().iter().take(3).map(|h| fixture.certificate(h)).collect();

        let result = tx_parents.send((parents, 1, 0)).await;
        assert!(result.is_ok());
        assert!(rx_ack.await.is_ok());

        // Ensure the proposer makes a correct header from the provided payload.
        let header = rx_headers.recv().await.unwrap();
        assert_eq!(header.payload().get(&digest), Some(&(worker_id, created_at_ts)));
        assert!(header.validate(&committee, &worker_cache).is_ok());

        // restart the proposer.
        tx_shutdown.send().unwrap();
        assert!(proposer_handle.await.is_ok());

        let mut tx_shutdown = PreSubscribedBroadcastSender::new(NUM_SHUTDOWN_RECEIVERS);
        let (tx_parents, rx_parents) = lattice_test_utils::test_channel!(1);
        let (tx_our_digests, rx_our_digests) = lattice_test_utils::test_channel!(1);
        let (tx_headers, mut rx_headers) = lattice_test_utils::test_channel!(1);
        let (tx_narwhal_round_updates, _rx_narwhal_round_updates) = watch::channel(0u64);
        let (_tx_committed_own_headers, rx_committed_own_headers) =
            lattice_test_utils::test_channel!(1);
        let metrics = Arc::new(PrimaryMetrics::new(&Registry::new()));
        let client = NetworkClient::new_from_keypair(&primary.network_keypair(), &primary.engine_network_keypair().public());

        // mock engine header provider
        let mut mock_engine = MockPrimaryToEngine::new();
        mock_engine.expect_build_header().return_once(
            move |_request| {
                tracing::debug!("mock engine expect_build_header: {_request:?}");
                let header = tn_types::execution::Header::default();
                Ok(
                    anemo::Response::new(
                        HeaderPayloadResponse {
                            sealed_header: header.seal_slow(),
                        }
                    )
                )
            }
        );

        client.set_primary_to_engine_local_handler(Arc::new(mock_engine));

        let _proposer_handle = Proposer::spawn(
            authority_id,
            committee.clone(),
            proposer_store,
            /* header_num_of_batches_threshold */ 1,
            /* max_header_num_of_batches */ 10,
            /* max_header_delay */
            Duration::from_millis(1_000_000), // Ensure it is not triggered.
            /* min_header_delay */
            Duration::from_millis(1_000_000), // Ensure it is not triggered.
            None,
            tx_shutdown.subscribe(),
            /* rx_core */ rx_parents,
            /* rx_workers */ rx_our_digests,
            /* tx_core */ tx_headers,
            tx_narwhal_round_updates,
            rx_committed_own_headers,
            metrics,
            client,
        );

        // Send enough digests for the header payload.
        let mut b = [0u8; 32];
        let r: Vec<u8> = (0..32).map(|_v| rand::random::<u8>()).collect();
        b.copy_from_slice(r.as_slice());

        let digest = BatchDigest(b);
        let worker_id = 0;
        let (tx_ack, rx_ack) = tokio::sync::oneshot::channel();
        tx_our_digests
            .send(OurDigestMessage { digest, worker_id, timestamp: 0, ack_channel: Some(tx_ack) })
            .await
            .unwrap();

        // Create and send a superset parents, same round but different set from before
        let parents: Vec<_> =
            fixture.headers().iter().take(4).map(|h| fixture.certificate(h)).collect();

        let result = tx_parents.send((parents, 1, 0)).await;
        assert!(result.is_ok());
        assert!(rx_ack.await.is_ok());

        // Ensure the proposer makes the same header as before
        let new_header = rx_headers.recv().await.unwrap();
        if new_header.round() == header.round() {
            assert_eq!(header, new_header);
        }
    }
}
