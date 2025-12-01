use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Display},
    time::SystemTime,
};

use anyhow::{Context, Result};
use frost_ed25519::{
    Identifier, SigningPackage, aggregate,
    keys::{KeyPackage, PublicKeyPackage},
    round1::{self, SigningCommitments, SigningNonces},
    round2::{self, SignatureShare},
};
use futures::future::join_all;
use minicbor::{Decode, Encode};
use rand::thread_rng;
use rust_decimal::prelude::ToPrimitive;
use tokio::sync::{mpsc, watch};
use tracing::{Instrument, Span, info, info_span, instrument, warn};

use crate::{
    cbor::{CborIdentifier, CborSignatureShare, CborSigningCommitments, CborSigningPackage},
    network::{NetworkSender, NodeId},
    price_feed::{
        GenericEntry, GenericPriceFeed, PlutusCompatible, PriceData, Signed, SignedEntries,
        SyntheticEntry, SyntheticPriceFeed, deserialize, serialize,
    },
    raft::RaftLeader,
    signature_aggregator::price_comparator::choose_synth_feeds_to_sign,
};

use super::{
    price_comparator::{ComparisonResult, choose_generic_feeds_to_sign},
    price_instrumentation::PriceInstrumentation,
};

#[derive(Decode, Encode, Clone, Debug)]
pub struct SignerMessage {
    #[n(0)]
    pub round: String,
    #[n(1)]
    pub data: SignerMessageData,
}
impl Display for SignerMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "{{round:{},message={}}}",
            self.round, self.data
        ))
    }
}

#[derive(Decode, Encode, Clone, Debug)]
pub enum SignerMessageData {
    #[n(0)]
    RequestCommitment(#[n(0)] CommitmentRequest),
    #[n(1)]
    Commit(#[n(0)] Commitment),
    #[n(2)]
    RequestSignature(#[n(0)] SignatureRequest),
    #[n(3)]
    Sign(#[n(0)] Signature),
    #[n(4)]
    Publish(#[n(0)] SignedEntries),
}
impl Display for SignerMessageData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequestCommitment(_) => f.write_str("RequestCommitment"),
            Self::Commit(_) => f.write_str("Commit"),
            Self::RequestSignature(_) => f.write_str("RequestSignature"),
            Self::Sign(_) => f.write_str("Sign"),
            Self::Publish(_) => f.write_str("Publish"),
        }
    }
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct CommitmentRequest {
    #[n(0)]
    synthetic_feeds: Vec<SyntheticPriceFeed>,
    #[n(1)]
    #[cbor(default)]
    generic_feeds: Vec<GenericPriceFeed>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct Commitment {
    #[n(0)]
    pub identifier: CborIdentifier,
    #[n(1)]
    pub commitments: BTreeMap<String, CborSigningCommitments>,
    #[n(2)]
    pub synthetic_feeds: Vec<SyntheticPriceFeed>,
    #[n(3)]
    #[cbor(default)]
    pub generic_feeds: Vec<GenericPriceFeed>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct SignatureRequest {
    #[n(0)]
    synthetic_packages: BTreeMap<String, CborSigningPackage>,
    #[n(1)]
    #[cbor(default)]
    generic_packages: BTreeMap<String, CborSigningPackage>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct Signature {
    #[n(0)]
    pub identifier: CborIdentifier,
    #[n(1)]
    pub signatures: BTreeMap<String, CborSignatureShare>,
}

#[derive(Clone)]
pub enum SignerEvent {
    /// We are the leader, and a new round of distributed signing has begun.
    RoundStarted(String),
    /// We are the leader, and the "grace period" for the current round is over.
    /// We want to wrap up this round ASAP, even if we don't have enough signatures for everything.
    RoundGracePeriodEnded(String),
    /// A new message has arrived from another node.
    Message(NodeId, SignerMessage, Span),
    /// There is a new leader (maybe this node, maybe another node, maybe we don't know).
    LeaderChanged(RaftLeader),
}
impl Display for SignerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RoundStarted(round) => {
                f.write_fmt(format_args!("RoundStarted{{round={}}}", round))
            }
            Self::RoundGracePeriodEnded(round) => {
                f.write_fmt(format_args!("RoundGracePeriodEnded{{round={}}}", round))
            }
            Self::Message(from, message, _) => {
                f.write_fmt(format_args!("Message{{from={},data={}}}", from, message))
            }
            Self::LeaderChanged(leader) => {
                f.write_fmt(format_args!("LeaderChanged{{{:?}}}", leader))
            }
        }
    }
}
impl SignerEvent {
    fn name(&self) -> &str {
        match self {
            Self::RoundStarted(_) => "RoundStarted",
            Self::RoundGracePeriodEnded(_) => "RoundGracePeriodEnded",
            Self::Message(_, _, _) => "Message",
            Self::LeaderChanged(_) => "LeaderChanged",
        }
    }
}

enum LeaderState {
    Ready,
    CollectingCommitments {
        round: String,
        round_span: Span,
        price_data: PriceData,
        commitments: Vec<(NodeId, Commitment)>,
        my_nonces: BTreeMap<String, SigningNonces>,
    },
    CollectingSignatures {
        round: String,
        round_span: Span,
        price_data: PriceData,
        signatures: Vec<Signature>,
        packages: BTreeMap<String, SigningPackage>,
        waiting_for: BTreeSet<NodeId>,
    },
}
impl Display for LeaderState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready => f.write_str("{role=Leader,state=Ready}"),
            Self::CollectingCommitments {
                round, commitments, ..
            } => {
                let committed: BTreeSet<Identifier> = commitments
                    .iter()
                    .map(|(_, c)| c.identifier.into())
                    .collect();
                f.write_fmt(format_args!(
                    "{{role=Leader,state=CollectingCommitments,round={},committed={:?}}}",
                    round, committed
                ))
            }
            Self::CollectingSignatures {
                round,
                signatures,
                packages,
                waiting_for,
                ..
            } => {
                let committed: BTreeSet<_> = packages
                    .values()
                    .flat_map(|p| p.signing_commitments().keys())
                    .collect();
                let signed: BTreeSet<_> = signatures.iter().map(|s| s.identifier).collect();
                f.write_fmt(format_args!("{{role=Leader,state=CollectingSignatures,round={},committed={:?},signed={:?},waiting_for={:?}}}", round, committed, signed, waiting_for))
            }
        }
    }
}
impl LeaderState {
    fn round(&self) -> &str {
        match self {
            Self::Ready => "",
            Self::CollectingCommitments { round, .. } => round,
            Self::CollectingSignatures { round, .. } => round,
        }
    }
}

enum FollowerState {
    Ready,
    Committed {
        round: String,
        round_span: Span,
        price_data: PriceData,
        nonces: BTreeMap<String, SigningNonces>,
    },
}
impl Display for FollowerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready => f.write_str("{role=Follower,state=Ready}"),
            Self::Committed { round, .. } => f.write_fmt(format_args!(
                "{{role=Follower,state=Committed,round={}}}",
                round
            )),
        }
    }
}
impl FollowerState {
    fn round(&self) -> &str {
        match self {
            Self::Ready => "",
            Self::Committed { round, .. } => round,
        }
    }
}

enum SignerState {
    Leader(LeaderState),
    Follower(NodeId, FollowerState),
    Unknown,
}
impl Display for SignerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Leader(state) => state.fmt(f),
            Self::Follower(_, state) => state.fmt(f),
            Self::Unknown => f.write_str("{role=Unknown}"),
        }
    }
}
impl SignerState {
    pub fn round(&self) -> &str {
        match self {
            Self::Leader(state) => state.round(),
            Self::Follower(_, state) => state.round(),
            Self::Unknown => "",
        }
    }
}

pub struct Signer {
    id: NodeId,
    key: KeyPackage,
    public_key: PublicKeyPackage,
    price_source: watch::Receiver<PriceData>,
    message_sink: NetworkSender<SignerMessage>,
    signed_entries_sink: mpsc::Sender<(NodeId, SignedEntries)>,
    state: SignerState,
    price_tracker: PriceInstrumentation,
    latest_payload: Option<SignedEntries>,
}

impl Signer {
    pub fn new(
        id: NodeId,
        key: KeyPackage,
        public_key: PublicKeyPackage,
        price_source: watch::Receiver<PriceData>,
        message_sink: NetworkSender<SignerMessage>,
        signed_entries_sink: mpsc::Sender<(NodeId, SignedEntries)>,
    ) -> Self {
        Self {
            id,
            key,
            public_key,
            price_source,
            message_sink,
            signed_entries_sink,
            state: SignerState::Unknown,
            price_tracker: PriceInstrumentation::default(),
            latest_payload: None,
        }
    }

    #[instrument(name = "process_signer", skip_all, fields(round = self.state.round(), state = %self.state))]
    pub async fn process(&mut self, event: SignerEvent) {
        info!(%event, "Started {} event", event.name());
        match self.do_process(event.clone()).await {
            Ok(()) => {
                info!(%event, "Finished event: {}", event.name())
            }
            Err(error) => {
                warn!(%error, "Error occurred during signing flow");
            }
        }
    }

    async fn do_process(&mut self, event: SignerEvent) -> Result<()> {
        match event {
            SignerEvent::LeaderChanged(leader) => {
                self.state = match leader {
                    RaftLeader::Myself => SignerState::Leader(LeaderState::Ready),
                    RaftLeader::Other(id) => SignerState::Follower(id, FollowerState::Ready),
                    RaftLeader::Unknown => SignerState::Unknown,
                };
                self.price_tracker.end_round();
            }
            SignerEvent::RoundStarted(round) => {
                if matches!(self.state, SignerState::Leader(_)) {
                    self.finish_round_early().await?;
                    self.begin_round(round).await?;
                }
            }
            SignerEvent::RoundGracePeriodEnded(round) => {
                if matches!(self.state, SignerState::Leader(_)) {
                    self.finish_collecting_commitments(round).await?;
                }
            }
            SignerEvent::Message(from, SignerMessage { round, data }, span) => match data {
                SignerMessageData::RequestCommitment(request) => {
                    self.send_commitment(round, request)
                        .instrument(span)
                        .await?;
                }
                SignerMessageData::Commit(commitment) => {
                    self.process_commitment(round, from, commitment)
                        .instrument(span)
                        .await?;
                }
                SignerMessageData::RequestSignature(request) => {
                    self.send_signature(round, request).instrument(span).await?;
                }
                SignerMessageData::Sign(signature) => {
                    self.process_signature(round, from, signature)
                        .instrument(span)
                        .await?;
                }
                SignerMessageData::Publish(payload) => {
                    self.publish(from, payload).instrument(span).await?;
                }
            },
        }
        Ok(())
    }

    async fn finish_round_early(&mut self) -> Result<()> {
        let round = match &self.state {
            SignerState::Leader(LeaderState::CollectingCommitments { round, .. }) => round,
            SignerState::Leader(LeaderState::CollectingSignatures { round, .. }) => round,
            _ => { return Ok(()) }
        };
        info!(round, "Finishing round before collecting enough signatures");
        let now = SystemTime::now();
        let payload = SignedEntries { timestamp: now, synthetics: vec![], generics: vec![] };
        self.publish(self.id.clone(), payload).await
    }

    async fn begin_round(&mut self, round: String) -> Result<()> {
        let price_data = self.price_source.borrow().clone();
        self.price_tracker.begin_round(&round, &price_data);

        let round_span = info_span!(
            "signer_round",
            role = "leader",
            round,
            price_data = format!("{:#?}", price_data)
        );
        round_span.follows_from(Span::current());

        round_span
            .in_scope(|| info!("Beginning round of signature collection, requesting commitments"));

        let synthetic_feeds: Vec<_> = price_data
            .synthetics
            .iter()
            .map(|entry| entry.feed.clone())
            .collect();
        let generic_feeds = price_data.generics.clone();
        let request = CommitmentRequest {
            synthetic_feeds: synthetic_feeds.clone(),
            generic_feeds: generic_feeds.clone(),
        };

        // request other folks commit
        self.message_sink
            .broadcast(SignerMessage {
                round: round.clone(),
                data: SignerMessageData::RequestCommitment(request),
            })
            .await;

        // And commit ourself
        let all_feeds: Vec<_> = synthetic_feeds
            .iter()
            .map(|s| s.synthetic.clone())
            .chain(generic_feeds.iter().map(|g| g.name.clone()))
            .collect();
        let (my_nonces, commitment) = self.commit(&all_feeds, synthetic_feeds, generic_feeds);
        self.state = SignerState::Leader(LeaderState::CollectingCommitments {
            round,
            round_span,
            price_data,
            commitments: vec![(self.id.clone(), commitment)],
            my_nonces,
        });

        Ok(())
    }

    async fn send_commitment(&mut self, round: String, request: CommitmentRequest) -> Result<()> {
        let SignerState::Follower(leader_id, old_state) = &self.state else {
            // we're not a follower ATM
            return Ok(());
        };
        let leader_id = leader_id.clone();
        let (round_span, price_data) = match old_state {
            FollowerState::Committed {
                round: curr_round,
                round_span,
                price_data,
                ..
            } if curr_round == &round => (round_span.clone(), price_data.clone()),
            _ => {
                let price_data = self.price_source.borrow().clone();
                let round_span = info_span!(
                    "signer_round",
                    role = "follower",
                    round,
                    price_data = format!("{:#?}", price_data)
                );
                round_span.follows_from(Span::current());
                (round_span, price_data)
            }
        };

        let mut feeds_to_sign = vec![];
        let my_synthetic_feeds: Vec<SyntheticPriceFeed> = price_data
            .synthetics
            .iter()
            .map(|entry| entry.feed.clone())
            .collect();
        for (synthetic, result) in
            choose_synth_feeds_to_sign(&request.synthetic_feeds, &my_synthetic_feeds)
        {
            match result {
                ComparisonResult::Sign => {
                    feeds_to_sign.push(synthetic.to_string());
                }
                ComparisonResult::DoNotSign(reason) => {
                    round_span.in_scope(|| {
                        warn!(
                            synthetic,
                            error = reason,
                            "Not sending commitment for this synthetic in this round"
                        )
                    });
                }
            }
        }

        let my_generic_feeds = price_data.generics.clone();
        for (feed, result) in
            choose_generic_feeds_to_sign(&request.generic_feeds, &my_generic_feeds)
        {
            match result {
                ComparisonResult::Sign => {
                    feeds_to_sign.push(feed.to_string());
                }
                ComparisonResult::DoNotSign(reason) => {
                    round_span.in_scope(|| {
                        warn!(
                            feed,
                            error = reason,
                            "Not sending commitment for this feed in this round"
                        )
                    });
                }
            }
        }

        // Send our commitment
        round_span.in_scope(|| info!("Sending commitment for this round"));
        let (nonces, commitment) =
            self.commit(&feeds_to_sign, my_synthetic_feeds, my_generic_feeds);
        self.message_sink
            .send(
                leader_id.clone(),
                SignerMessage {
                    round: round.clone(),
                    data: SignerMessageData::Commit(commitment),
                },
            )
            .await;

        // And wait to be asked to sign
        self.state = SignerState::Follower(
            leader_id.clone(),
            FollowerState::Committed {
                round,
                round_span,
                price_data,
                nonces,
            },
        );

        Ok(())
    }

    async fn process_commitment(
        &mut self,
        round: String,
        from: NodeId,
        commitment: Commitment,
    ) -> Result<()> {
        self.price_tracker
            .track_prices(&round, &commitment.synthetic_feeds);

        let SignerState::Leader(LeaderState::CollectingCommitments {
            round: curr_round,
            round_span,
            commitments,
            ..
        }) = &mut self.state
        else {
            // we're not the leader ATM, or not waiting for commitments
            return Ok(());
        };
        if *curr_round != round {
            // Commitment from a different round
            return Ok(());
        }
        let round_span = round_span.clone();

        if commitments
            .iter()
            .any(|(_, c)| c.identifier == commitment.identifier)
        {
            // we saw this one
            return Ok(());
        }
        commitments.push((from, commitment));

        if needs_more_commitments(commitments, &self.key, &self.public_key) {
            // still collecting
            return Ok(());
        }

        round_span.in_scope(|| {
            info!("Collected enough commitments!");
        });

        self.finish_collecting_commitments(round).await?;

        Ok(())
    }

    async fn finish_collecting_commitments(&mut self, round: String) -> Result<()> {
        let SignerState::Leader(LeaderState::CollectingCommitments {
            round: curr_round,
            round_span,
            price_data,
            commitments,
            my_nonces,
        }) = &mut self.state
        else {
            // we're not the leader ATM, or not waiting for commitments
            return Ok(());
        };
        if *curr_round != round {
            // Just double-checking that we're collecting sigs for the right round
            return Ok(());
        }
        let round_span = round_span.clone();

        let mut commitment_maps = BTreeMap::new();
        let mut recipients = vec![];
        // time to transition to signing
        for (signer, commitment) in commitments {
            if *signer != self.id {
                recipients.push(signer.clone());
            }
            for (synthetic, c) in commitment.commitments.iter() {
                let comm: SigningCommitments = (*c).into();
                commitment_maps
                    .entry(synthetic.clone())
                    .or_insert(BTreeMap::new())
                    .insert(commitment.identifier.into(), comm);
            }
        }

        round_span.in_scope(|| info!(?recipients, "Time to collect signatures"));

        // Request signatures from all the committees
        let price_data = price_data.clone();
        let mut packages = BTreeMap::new();
        let synthetic_packages: BTreeMap<String, CborSigningPackage> = price_data
            .synthetics
            .iter()
            .filter_map(|entry| {
                let synthetic = &entry.feed.synthetic;
                let message = serialize(&entry.feed);
                let commitments = commitment_maps.remove(synthetic)?;
                if commitments.len() < (*self.key.min_signers() as usize) {
                    // Don't have enough commitments for this particular package.
                    return None;
                }
                let package = SigningPackage::new(commitments, &message);
                packages.insert(synthetic.clone(), package.clone());
                Some((synthetic.clone(), package.into()))
            })
            .collect();
        let generic_packages: BTreeMap<String, CborSigningPackage> = price_data
            .generics
            .iter()
            .filter_map(|entry| {
                let feed = &entry.name;
                let message = serialize(entry);
                let commitments = commitment_maps.remove(feed)?;
                if commitments.len() < (*self.key.min_signers() as usize) {
                    // Don't have enough commitments for this particular package.
                    return None;
                }
                let package = SigningPackage::new(commitments, &message);
                packages.insert(feed.clone(), package.clone());
                Some((feed.clone(), package.into()))
            })
            .collect();
        let send_to_recipient_tasks = recipients.iter().cloned().map(|recipient| {
            let message = SignerMessage {
                round: round.clone(),
                data: SignerMessageData::RequestSignature(SignatureRequest {
                    synthetic_packages: synthetic_packages.clone(),
                    generic_packages: generic_packages.clone(),
                }),
            };
            self.message_sink.send(recipient, message)
        });
        join_all(send_to_recipient_tasks).await;

        // And be sure to sign the message ourself
        let nonces = my_nonces.clone();
        let my_signature = self.sign(&packages, &nonces)?;

        self.state = SignerState::Leader(LeaderState::CollectingSignatures {
            round,
            round_span,
            price_data,
            signatures: vec![my_signature],
            packages,
            waiting_for: recipients.into_iter().collect(),
        });

        Ok(())
    }

    async fn send_signature(&mut self, round: String, request: SignatureRequest) -> Result<()> {
        let synthetic_packages: BTreeMap<String, SigningPackage> = request
            .synthetic_packages
            .into_iter()
            .map(|(synthetic, i)| (synthetic, i.into()))
            .collect();
        let generic_packages: BTreeMap<String, SigningPackage> = request
            .generic_packages
            .into_iter()
            .map(|(feed, i)| (feed, i.into()))
            .collect();
        let SignerState::Follower(
            _,
            FollowerState::Committed {
                round: curr_round,
                round_span,
                ..
            },
        ) = &self.state
        else {
            // We're not a follower, or we aren't waiting to sign something
            return Ok(());
        };
        if *curr_round != round {
            // Signature requested from a different round
            return Ok(());
        };
        let round_span = round_span.clone();

        match self
            .do_send_signature(round, synthetic_packages, generic_packages)
            .instrument(round_span.clone())
            .await
        {
            Ok(_) => Ok(()),
            Err(error) => {
                round_span.in_scope(|| warn!(?error));
                Err(error)
            }
        }
    }

    // separated into its own method so that we can easily instrument the error
    async fn do_send_signature(
        &mut self,
        round: String,
        mut synthetic_packages: BTreeMap<String, SigningPackage>,
        mut generic_packages: BTreeMap<String, SigningPackage>,
    ) -> Result<()> {
        let SignerState::Follower(
            leader_id,
            FollowerState::Committed {
                price_data, nonces, ..
            },
        ) = &self.state
        else {
            return Ok(());
        };

        // Find the set which we are OK with signing
        let mut packages = BTreeMap::new();

        let leader_feed = self.parse_packages(&synthetic_packages)?;
        let my_feed: Vec<_> = price_data
            .synthetics
            .iter()
            .map(|entry| entry.feed.clone())
            .collect();

        for (synthetic, result) in choose_synth_feeds_to_sign(&leader_feed, &my_feed) {
            match result {
                ComparisonResult::Sign => {
                    let package = synthetic_packages.remove(synthetic).unwrap();
                    packages.insert(synthetic.to_string(), package);
                }
                ComparisonResult::DoNotSign(reason) => {
                    warn!(
                        synthetic,
                        error = reason,
                        "Not signing this synthetic in this round"
                    );
                }
            }
        }

        let leader_feed = self.parse_packages(&generic_packages)?;
        let my_feed = price_data.generics.clone();

        for (feed, result) in choose_generic_feeds_to_sign(&leader_feed, &my_feed) {
            match result {
                ComparisonResult::Sign => {
                    let package = generic_packages.remove(feed).unwrap();
                    packages.insert(feed.to_string(), package);
                }
                ComparisonResult::DoNotSign(reason) => {
                    warn!(feed, error = reason, "Not signing this feed in this round");
                }
            }
        }

        // Send our signature back to the leader
        let signature = self.sign(&packages, nonces)?;
        self.message_sink
            .send(
                leader_id.clone(),
                SignerMessage {
                    round,
                    data: SignerMessageData::Sign(signature),
                },
            )
            .await;
        info!("Sent signature for this round");

        // and vibe until we get back into the signature flow
        self.state = SignerState::Follower(leader_id.clone(), FollowerState::Ready);

        Ok(())
    }

    fn parse_packages<T: PlutusCompatible>(
        &self,
        packages: &BTreeMap<String, SigningPackage>,
    ) -> Result<Vec<T>> {
        packages
            .values()
            .map(|p| deserialize(p.message()).context("could not decode value"))
            .collect()
    }

    async fn process_signature(
        &mut self,
        round: String,
        from: NodeId,
        signature: Signature,
    ) -> Result<()> {
        let SignerState::Leader(LeaderState::CollectingSignatures {
            round: curr_round,
            round_span,
            price_data,
            signatures,
            packages,
            waiting_for,
        }) = &mut self.state
        else {
            // We're not a leader, or we're not waiting for signatures
            return Ok(());
        };
        if *curr_round != round {
            // Signature from another round
            return Ok(());
        }

        if signatures
            .iter()
            .any(|s| s.identifier == signature.identifier)
        {
            // we saw this one
            return Ok(());
        }
        signatures.push(signature);
        waiting_for.remove(&from);

        if !waiting_for.is_empty() {
            // still collecting
            return Ok(());
        }

        round_span
            .in_scope(|| info!("Collected all signatures, signing and completing this round"));

        // We've finally collected all the signatures we need! Now just aggregate them
        let mut signature_maps = BTreeMap::new();
        for signature in signatures {
            for (synthetic, s) in signature.signatures.iter() {
                let sig: SignatureShare = (*s).into();
                signature_maps
                    .entry(synthetic.clone())
                    .or_insert(BTreeMap::new())
                    .insert(signature.identifier.into(), sig);
            }
        }

        let mut signatures = BTreeMap::new();
        for (synthetic, package) in packages.iter() {
            let Some(sigs) = signature_maps.get(synthetic) else {
                continue;
            };
            if sigs.len() < (*self.key.min_signers() as usize) {
                // Don't have enough signatures for this particular entry
                continue;
            }
            let signature = aggregate(package, sigs, &self.public_key)?;
            signatures.insert(synthetic.clone(), signature.serialize()?);
        }

        let now = SystemTime::now();
        let mut synthetic_entries = BTreeMap::new();
        for entry in price_data.synthetics.iter() {
            // If we don't have a signature in the map, we're not signing this price feed
            let Some(signature) = signatures.remove(&entry.feed.synthetic) else {
                continue;
            };
            synthetic_entries.insert(
                entry.feed.synthetic.clone(),
                SyntheticEntry {
                    price: entry.price.to_f64().expect("Could not convert decimal"),
                    feed: Signed {
                        data: entry.feed.clone(),
                        signature,
                    },
                    timestamp: Some(now),
                },
            );
        }
        let mut generic_entries = BTreeMap::new();
        for entry in price_data.generics.iter() {
            // If we don't have a signature in the map, we're not signing this price feed
            let Some(signature) = signatures.remove(&entry.name) else {
                continue;
            };
            generic_entries.insert(
                entry.name.clone(),
                GenericEntry {
                    feed: Signed {
                        data: entry.clone(),
                        signature,
                    },
                    timestamp: now,
                },
            );
        }

        // If there's any synthetics which were in a previous round but not this one,
        // include them in the payload as well.
        if let Some(old_payload) = self.latest_payload.as_ref() {
            for entry in &old_payload.synthetics {
                let synthetic = &entry.feed.data.synthetic;
                if !synthetic_entries.contains_key(synthetic) {
                    synthetic_entries.insert(synthetic.clone(), entry.clone());
                }
            }
        }

        let payload = SignedEntries {
            timestamp: now,
            synthetics: synthetic_entries.into_values().collect(),
            generics: generic_entries.into_values().collect(),
        };

        self.publish(self.id.clone(), payload.clone()).await?;

        // Notify our peers that we've agreed to sign the payload
        self.message_sink
            .broadcast(SignerMessage {
                round,
                data: SignerMessageData::Publish(payload),
            })
            .await;

        // And now that we've signed something, wait for the next round
        self.state = SignerState::Leader(LeaderState::Ready);

        Ok(())
    }

    async fn publish(&mut self, publisher: NodeId, payload: SignedEntries) -> Result<()> {
        for entry in &payload.synthetics {
            self.validate_feed_signature(&entry.feed)
                .context("tried to publish feed with invalid signature")?;
        }
        for entry in &payload.generics {
            self.validate_feed_signature(&entry.feed)
                .context("tried to publish feed with invalid signature")?;
        }
        self.signed_entries_sink
            .send((publisher, payload.clone()))
            .await
            .context("Could not publish signed result")?;

        if self
            .latest_payload
            .as_ref()
            .is_none_or(|old| old.timestamp <= payload.timestamp)
        {
            self.latest_payload = Some(payload);
        }

        Ok(())
    }

    fn commit(
        &self,
        synthetics: &[String],
        synthetic_feeds: Vec<SyntheticPriceFeed>,
        generic_feeds: Vec<GenericPriceFeed>,
    ) -> (BTreeMap<String, SigningNonces>, Commitment) {
        let mut nonces = BTreeMap::new();
        let mut commitments = BTreeMap::new();
        for synthetic in synthetics {
            let (nonce, commitment) = round1::commit(self.key.signing_share(), &mut thread_rng());
            nonces.insert(synthetic.to_string(), nonce);
            commitments.insert(synthetic.to_string(), commitment.into());
        }
        let commitment = Commitment {
            identifier: (*self.key.identifier()).into(),
            commitments,
            synthetic_feeds,
            generic_feeds,
        };
        (nonces, commitment)
    }

    fn sign(
        &self,
        packages: &BTreeMap<String, SigningPackage>,
        nonces: &BTreeMap<String, SigningNonces>,
    ) -> Result<Signature> {
        let mut signatures = BTreeMap::new();
        for (feed, nonce) in nonces {
            if let Some(package) = packages.get(feed) {
                let signature = round2::sign(package, nonce, &self.key)?;
                signatures.insert(feed.clone(), signature.into());
            };
        }
        Ok(Signature {
            identifier: (*self.key.identifier()).into(),
            signatures,
        })
    }

    fn validate_feed_signature<T: PlutusCompatible>(&self, feed: &Signed<T>) -> Result<()> {
        let data = serialize(&feed.data);
        let signature = frost_ed25519::Signature::deserialize(&feed.signature)?;
        self.key.verifying_key().verify(&data, &signature)?;
        Ok(())
    }
}

fn needs_more_commitments(
    commitments: &[(NodeId, Commitment)],
    key: &KeyPackage,
    public_key: &PublicKeyPackage,
) -> bool {
    let min_signers = *key.min_signers() as usize;
    let max_signers = public_key.verifying_shares().len();
    if commitments.len() < min_signers {
        // If not enough nodes have committed, we can't sign anything
        return true;
    }
    if commitments.len() == max_signers {
        // If every node has committed, there's no point in waiting for more commitments.
        // Just collect as many signatures as we can.
        return false;
    }
    let mut commitment_counts = BTreeMap::new();
    for (_, commitment) in commitments.iter() {
        for synthetic in commitment.commitments.keys() {
            *commitment_counts.entry(synthetic).or_default() += 1;
        }
    }

    // If we have enough commitments for every synthetic, we're done!
    // Note that we can assume there's at least one commitment for every
    // synthetic, because as the leader we already committed to signing everything
    commitment_counts
        .into_values()
        .any(|count: usize| count < min_signers)
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use frost_ed25519::keys::{self, IdentifierList, KeyPackage};
    use num_bigint::BigUint;
    use num_rational::BigRational;
    use num_traits::FromPrimitive as _;
    use rand::thread_rng;
    use tokio::sync::{mpsc, watch};
    use tracing::{Instrument, Span};

    use crate::{
        network::{NetworkReceiver, NodeId, TestNetwork},
        price_feed::{
            GenericPriceFeed, PriceData, SignedEntries, SyntheticPriceData, SyntheticPriceFeed,
            Validity,
        },
        raft::RaftLeader,
        signature_aggregator::signer::SignerMessage,
    };

    use super::{Signer, SignerEvent};

    struct SignerOrchestrator {
        id: NodeId,
        signer: Signer,
        receiver: NetworkReceiver<SignerMessage>,
    }

    impl SignerOrchestrator {
        async fn process(&mut self, event: SignerEvent) {
            self.signer.process(event).await;
        }
        async fn process_incoming_messages(&mut self) {
            while let Some(message) = self.receiver.try_recv().await {
                self.signer
                    .process(SignerEvent::Message(
                        message.from,
                        message.data,
                        Span::none(),
                    ))
                    .instrument(message.span)
                    .await;
            }
        }
    }

    async fn construct_signers(
        min: u16,
        prices: Vec<PriceData>,
    ) -> Result<(
        Vec<SignerOrchestrator>,
        TestNetwork<SignerMessage>,
        mpsc::Receiver<(NodeId, SignedEntries)>,
    )> {
        let rng = thread_rng();
        let (shares, public_key) =
            keys::generate_with_dealer(prices.len() as u16, min, IdentifierList::Default, rng)?;

        let mut network = TestNetwork::new();
        let (payload_sink, payload_source) = mpsc::channel(10);

        let signers: Vec<SignerOrchestrator> = shares
            .into_values()
            .zip(prices.into_iter())
            .map(|(share, price_data)| {
                let key = KeyPackage::try_from(share).unwrap();
                let (_, price_source) = watch::channel(price_data);
                let (id, channel) = network.connect();
                let (sender, receiver) = channel.split();
                let signer = Signer::new(
                    id.clone(),
                    key,
                    public_key.clone(),
                    price_source,
                    sender,
                    payload_sink.clone(),
                );
                SignerOrchestrator {
                    id,
                    signer,
                    receiver,
                }
            })
            .collect();

        Ok((signers, network, payload_source))
    }

    // make the first signer the leader, and other signers followers.
    // return the leader's id
    async fn assign_roles(signers: &mut [SignerOrchestrator]) -> NodeId {
        let leader_id = signers[0].id.clone();
        signers[0]
            .process(SignerEvent::LeaderChanged(RaftLeader::Myself))
            .await;
        for signer in signers.iter_mut().skip(1) {
            signer
                .process(SignerEvent::LeaderChanged(RaftLeader::Other(
                    leader_id.clone(),
                )))
                .await;
        }
        leader_id
    }

    async fn run_round(
        signers: &mut [SignerOrchestrator],
        network: &mut TestNetwork<SignerMessage>,
    ) {
        signers[0]
            .process(SignerEvent::RoundStarted("round".into()))
            .await;
        while network.drain().await {
            for signer in signers.iter_mut() {
                signer.process_incoming_messages().await;
            }
        }
        signers[0]
            .process(SignerEvent::RoundGracePeriodEnded("round".into()))
            .await;
        while network.drain().await {
            for signer in signers.iter_mut() {
                signer.process_incoming_messages().await;
            }
        }
    }

    fn assert_round_complete(
        payload_source: &mut mpsc::Receiver<(NodeId, SignedEntries)>,
    ) -> Result<SignedEntries> {
        match payload_source.try_recv() {
            Ok((_, entries)) => Ok(entries),
            Err(x) => Err(x.into()),
        }
    }

    fn synthetic_data(
        synthetic: &str,
        price: f64,
        collateral_names: &[&str],
        collateral_prices: &[u64],
        denominator: u64,
    ) -> SyntheticPriceData {
        SyntheticPriceData {
            price: BigRational::from_f64(price).unwrap(),
            feed: SyntheticPriceFeed {
                collateral_names: Some(collateral_names.iter().map(|&s| s.to_string()).collect()),
                collateral_prices: collateral_prices
                    .iter()
                    .map(|&p| BigUint::from(p))
                    .collect(),
                synthetic: synthetic.into(),
                denominator: BigUint::from(denominator),
                validity: Validity::default(),
            },
        }
    }

    fn generic_feed(name: &str, price: u64) -> GenericPriceFeed {
        GenericPriceFeed {
            price: BigUint::from(price),
            name: name.to_string(),
            timestamp: 1337,
        }
    }

    #[tokio::test]
    async fn should_sign_payload_with_quorum_of_two() -> Result<()> {
        let price_data = PriceData {
            synthetics: vec![
                synthetic_data("ADA", 3.50, &["A", "B"], &[3, 4], 1),
                synthetic_data("BTN", 1.21, &["A", "B"], &[5, 8], 1),
            ],
            generics: vec![generic_feed("ADA/USD#RAW", 6500000)],
        };

        let prices = vec![price_data.clone(); 3];
        let (mut signers, mut network, mut payload_source) = construct_signers(2, prices).await?;

        // set everyone's roles
        let leader_id = assign_roles(&mut signers).await;

        // Start the round
        signers[0]
            .process(SignerEvent::RoundStarted("round".into()))
            .await;

        // Leader should have broadcast a request to all followers
        let message = network.drain_one(&signers[0].id).await.remove(0);
        assert_eq!(message.to, None);

        // Make a single follower commit
        signers[1].process_incoming_messages().await;

        // Follower should have sent a commitment to the leader
        let message = network.drain_one(&signers[1].id).await.remove(0);
        assert_eq!(message.to, Some(leader_id.clone()));

        // Send that commitment to the leader
        signers[0].process_incoming_messages().await;

        // Leader should have sent a signing request to that follower
        let message = network.drain_one(&signers[0].id).await.remove(0);
        assert_eq!(message.to, Some(signers[1].id.clone()));

        // Send that request to the follower
        signers[1].process_incoming_messages().await;

        // Follower should have sent a signature to the leader
        let message = network.drain_one(&signers[1].id).await.remove(0);
        assert_eq!(message.to, Some(leader_id.clone()));

        // Send that signature to the leader
        signers[0].process_incoming_messages().await;

        // leader should have published results
        let signed_entries = assert_round_complete(&mut payload_source)?;
        assert_eq!(signed_entries.synthetics.len(), 2);
        assert_eq!(signed_entries.generics.len(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn should_sign_payload_with_quorum_of_three() -> Result<()> {
        let price_data = PriceData {
            synthetics: vec![
                synthetic_data("ADA", 3.50, &["A", "B"], &[3, 4], 1),
                synthetic_data("BTN", 1.21, &["A", "B"], &[5, 8], 1),
            ],
            generics: vec![
                generic_feed("ADA/USD#RAW", 6500000),
                generic_feed("ADA/USD#GEMA", 6600000),
            ],
        };

        let (mut signers, mut network, mut payload_source) =
            construct_signers(3, vec![price_data; 4]).await?;
        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        let signed_entries = assert_round_complete(&mut payload_source)?;
        assert_eq!(signed_entries.synthetics.len(), 2);

        Ok(())
    }

    #[tokio::test]
    async fn should_sign_close_enough_collateral_prices() -> Result<()> {
        let leader_prices = PriceData {
            synthetics: vec![synthetic_data("SYNTH", 3.50, &["A"], &[2], 1)],
            generics: vec![],
        };
        let follower_prices = PriceData {
            synthetics: vec![synthetic_data("SYNTH", 3.50, &["A"], &[199], 100)],
            generics: vec![],
        };

        let (mut signers, mut network, mut payload_source) =
            construct_signers(2, vec![leader_prices, follower_prices]).await?;
        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        let signed_entries = assert_round_complete(&mut payload_source)?;
        assert_eq!(signed_entries.synthetics.len(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn should_not_sign_distant_collateral_prices() -> Result<()> {
        let leader_prices = PriceData {
            synthetics: vec![synthetic_data("SYNTH", 3.50, &["A"], &[2], 1)],
            generics: vec![],
        };
        let follower_prices = PriceData {
            synthetics: vec![synthetic_data("SYNTH", 3.50, &["A"], &[3], 2)],
            generics: vec![],
        };

        let (mut signers, mut network, mut payload_source) =
            construct_signers(2, vec![leader_prices, follower_prices]).await?;
        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        let signed_entries = assert_round_complete(&mut payload_source)?;
        assert_eq!(signed_entries.synthetics.len(), 0);

        Ok(())
    }

    #[tokio::test]
    async fn should_sign_close_enough_generic_prices() -> Result<()> {
        let leader_prices = PriceData {
            synthetics: vec![],
            generics: vec![generic_feed("ADA/USD#RAW", 3500000)],
        };
        let follower_prices = PriceData {
            synthetics: vec![],
            generics: vec![generic_feed("ADA/USD#RAW", 3490000)],
        };

        let (mut signers, mut network, mut payload_source) =
            construct_signers(2, vec![leader_prices, follower_prices]).await?;
        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        let signed_entries = assert_round_complete(&mut payload_source)?;
        assert_eq!(signed_entries.generics.len(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn should_not_sign_distant_generic_prices() -> Result<()> {
        let leader_prices = PriceData {
            synthetics: vec![],
            generics: vec![generic_feed("ADA/USD#RAW", 3500000)],
        };
        let follower_prices = PriceData {
            synthetics: vec![],
            generics: vec![generic_feed("ADA/USD#RAW", 7000000)],
        };

        let (mut signers, mut network, mut payload_source) =
            construct_signers(2, vec![leader_prices, follower_prices]).await?;
        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        let signed_entries = assert_round_complete(&mut payload_source)?;
        assert_eq!(signed_entries.generics.len(), 0);

        Ok(())
    }

    #[tokio::test]
    async fn should_sign_some_collateral_prices_but_not_others() -> Result<()> {
        let leader_prices = PriceData {
            synthetics: vec![
                synthetic_data("GOOD", 3.50, &["A"], &[2], 1),
                synthetic_data("BAD", 3.50, &["A"], &[2], 1),
            ],
            generics: vec![],
        };
        let follower_prices = PriceData {
            synthetics: vec![
                synthetic_data("GOOD", 3.50, &["A"], &[2], 1),
                synthetic_data("BAD", 3.50, &["A"], &[3], 2),
            ],
            generics: vec![],
        };

        let (mut signers, mut network, mut payload_source) =
            construct_signers(2, vec![leader_prices, follower_prices]).await?;
        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        let signed_entries = assert_round_complete(&mut payload_source)?;
        assert_eq!(signed_entries.synthetics.len(), 1);
        assert_eq!(signed_entries.synthetics[0].feed.data.synthetic, "GOOD");

        Ok(())
    }

    #[tokio::test]
    async fn should_keep_collecting_commitments_if_not_all_synthetics_are_covered() -> Result<()> {
        let leader_prices = PriceData {
            synthetics: vec![
                synthetic_data("ALL_MATCH", 3.50, &["A"], &[2], 1),
                synthetic_data("SOME_MATCH", 3.50, &["A"], &[2], 1),
            ],
            generics: vec![],
        };
        let partially_matching_prices = PriceData {
            synthetics: vec![
                synthetic_data("ALL_MATCH", 3.50, &["A"], &[2], 1),
                synthetic_data("SOME_MATCH", 3.50, &["A"], &[3], 2),
            ],
            generics: vec![],
        };

        // Follower #1 has some, but not all, values in common with the leader.
        // Follower #2 has all values in common with the leader.
        // The network is deterministic in these tests; follower #1 will commit before it.
        let (mut signers, mut network, mut payload_source) = construct_signers(
            2,
            vec![
                leader_prices.clone(),
                partially_matching_prices,
                leader_prices,
            ],
        )
        .await?;
        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        let signed_entries = assert_round_complete(&mut payload_source)?;
        assert_eq!(signed_entries.synthetics.len(), 2);

        Ok(())
    }

    #[tokio::test]
    async fn should_stop_collecting_commitments_if_node_is_disconnected() -> Result<()> {
        let leader_prices = PriceData {
            synthetics: vec![
                synthetic_data("ALL_MATCH", 3.50, &["A"], &[2], 1),
                synthetic_data("SOME_MATCH", 3.50, &["A"], &[2], 1),
            ],
            generics: vec![],
        };
        let partially_matching_prices = PriceData {
            synthetics: vec![
                synthetic_data("ALL_MATCH", 3.50, &["A"], &[2], 1),
                synthetic_data("SOME_MATCH", 3.50, &["A"], &[3], 2),
            ],
            generics: vec![],
        };

        // Follower #1 has some, but not all, values in common with the leader.
        // Follower #2 is disconnected.
        let (mut signers, mut network, mut payload_source) = construct_signers(
            2,
            vec![
                leader_prices.clone(),
                partially_matching_prices,
                leader_prices,
            ],
        )
        .await?;
        network.disconnect(&signers[2].id);

        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        let signed_entries = assert_round_complete(&mut payload_source)?;
        assert_eq!(signed_entries.synthetics.len(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn should_collect_different_signatures_from_different_nodes() -> Result<()> {
        let leader_prices = PriceData {
            synthetics: vec![
                synthetic_data("FIRST", 3.50, &["A"], &[2], 1),
                synthetic_data("SECOND", 3.50, &["A"], &[2], 1),
            ],
            generics: vec![],
        };
        let follower1_prices = PriceData {
            synthetics: vec![
                synthetic_data("FIRST", 3.50, &["A"], &[2], 1),
                synthetic_data("SECOND", 3.50, &["A"], &[3], 2),
            ],
            generics: vec![],
        };
        let follower2_prices = PriceData {
            synthetics: vec![
                synthetic_data("FIRST", 3.50, &["A"], &[3], 2),
                synthetic_data("SECOND", 3.50, &["A"], &[2], 1),
            ],
            generics: vec![],
        };

        // Follower #1 agrees with the leader on one price, follower #2 on another.
        // We can only sign two payloads if we wait for both of them to finish.
        let (mut signers, mut network, mut payload_source) =
            construct_signers(2, vec![leader_prices, follower1_prices, follower2_prices]).await?;

        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        let signed_entries = assert_round_complete(&mut payload_source)?;
        assert_eq!(signed_entries.synthetics.len(), 2);

        Ok(())
    }

    #[tokio::test]
    async fn should_end_round_early_when_new_round_begins() -> Result<()> {
        let leader_prices = PriceData {
            synthetics: vec![
                synthetic_data("FIRST", 3.50, &["A"], &[2], 1),
                synthetic_data("SECOND", 3.50, &["A"], &[2], 1),
            ],
            generics: vec![],
        };
        let follower_prices = leader_prices.clone();
        let (mut signers, mut _network, mut payload_source) =
            construct_signers(2, vec![leader_prices, follower_prices]).await?;

        assign_roles(&mut signers).await;

        signers[0]
            .process(SignerEvent::RoundStarted("round1".into()))
            .await;

        signers[0]
            .process(SignerEvent::RoundStarted("round2".into()))
            .await;

        let signed_entries = assert_round_complete(&mut payload_source)?;
        assert_eq!(signed_entries.synthetics.len(), 0);
        assert_eq!(signed_entries.generics.len(), 0);

        Ok(())
    }
}
