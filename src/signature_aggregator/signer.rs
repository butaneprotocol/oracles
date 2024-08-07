use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Display},
    time::SystemTime,
};

use anyhow::{Context, Result};
use frost_ed25519::{
    aggregate,
    keys::{KeyPackage, PublicKeyPackage},
    round1::{self, SigningCommitments, SigningNonces},
    round2::{self, SignatureShare},
    Identifier, SigningPackage,
};
use futures::future::join_all;
use minicbor::{Decode, Encode};
use rand::thread_rng;
use rust_decimal::prelude::ToPrimitive;
use tokio::sync::{mpsc, watch};
use tracing::{info, info_span, instrument, warn, Instrument, Span};
use uuid::Uuid;

use crate::{
    cbor::{CborIdentifier, CborSignatureShare, CborSigningCommitments, CborSigningPackage},
    network::{NetworkSender, NodeId},
    price_feed::{
        deserialize, serialize, PriceFeed, PriceFeedEntry, SignedEntries, SignedEntry,
        SignedPriceFeed,
    },
    raft::RaftLeader,
    signature_aggregator::price_comparator::choose_feeds_to_sign,
};

use super::price_comparator::ComparisonResult;

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
    price_feed: Vec<PriceFeed>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct Commitment {
    #[n(0)]
    pub identifier: CborIdentifier,
    #[n(1)]
    pub commitments: BTreeMap<String, CborSigningCommitments>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct SignatureRequest {
    #[n(0)]
    packages: BTreeMap<String, CborSigningPackage>,
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
    RoundStarted,
    Message(NodeId, SignerMessage, Span),
    LeaderChanged(RaftLeader),
}
impl Display for SignerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RoundStarted => f.write_str("RoundStarted"),
            Self::Message(from, message, _) => {
                f.write_fmt(format_args!("Message{{from={},data={}}}", from, message))
            }
            Self::LeaderChanged(leader) => {
                f.write_fmt(format_args!("LeaderChanged{{{:?}}}", leader))
            }
        }
    }
}

enum LeaderState {
    Ready,
    CollectingCommitments {
        round: String,
        round_span: Span,
        price_data: Vec<PriceFeedEntry>,
        commitments: Vec<(NodeId, Commitment)>,
        my_nonces: BTreeMap<String, SigningNonces>,
    },
    CollectingSignatures {
        round: String,
        round_span: Span,
        price_data: Vec<PriceFeedEntry>,
        signatures: Vec<Signature>,
        packages: BTreeMap<String, SigningPackage>,
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
                ..
            } => {
                let committed: BTreeSet<_> = packages
                    .values()
                    .flat_map(|p| p.signing_commitments().keys())
                    .collect();
                let signed: BTreeSet<_> = signatures.iter().map(|s| s.identifier).collect();
                f.write_fmt(format_args!("{{role=Leader,state=CollectingSignatures,round={},committed={:?},signed={:?}}}", round, committed, signed))
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
        price_data: Vec<PriceFeedEntry>,
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
    price_source: watch::Receiver<Vec<PriceFeedEntry>>,
    message_sink: NetworkSender<SignerMessage>,
    signed_entries_sink: mpsc::Sender<(NodeId, SignedEntries)>,
    state: SignerState,
}

impl Signer {
    pub fn new(
        id: NodeId,
        key: KeyPackage,
        public_key: PublicKeyPackage,
        price_source: watch::Receiver<Vec<PriceFeedEntry>>,
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
        }
    }

    #[instrument(name = "process_signer", skip_all, fields(round = self.state.round(), state = %self.state))]
    pub async fn process(&mut self, event: SignerEvent) {
        info!("Started event: {}", event);
        match self.do_process(event.clone()).await {
            Ok(()) => {
                info!("Finished event: {}", event)
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
            }
            SignerEvent::RoundStarted => {
                if matches!(self.state, SignerState::Leader(_)) {
                    self.request_commitments().await?;
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
                    self.process_signature(round, signature)
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

    async fn request_commitments(&mut self) -> Result<()> {
        let round = Uuid::new_v4().to_string();
        let price_data = self.price_source.borrow().clone();

        let round_span = info_span!(
            "signer_round",
            role = "leader",
            round,
            price_data = format!("{:#?}", price_data)
        );
        round_span.follows_from(Span::current());

        round_span
            .in_scope(|| info!("Beginning round of signature collection, requesting commitments"));

        let price_feed = price_data.iter().map(|entry| entry.data.clone()).collect();
        let request = CommitmentRequest { price_feed };

        // request other folks commit
        self.message_sink
            .broadcast(SignerMessage {
                round: round.clone(),
                data: SignerMessageData::RequestCommitment(request),
            })
            .await;

        // And commit ourself
        let all_synthetics: Vec<_> = price_data
            .iter()
            .map(|entry| entry.data.synthetic.as_str())
            .collect();
        let (my_nonces, commitment) = self.commit(&all_synthetics);
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

        let my_feed: Vec<PriceFeed> = price_data.iter().map(|entry| entry.data.clone()).collect();
        let mut synthetics_to_sign = vec![];
        for (synthetic, result) in choose_feeds_to_sign(&request.price_feed, &my_feed) {
            match result {
                ComparisonResult::Sign => {
                    synthetics_to_sign.push(synthetic);
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

        // Send our commitment
        round_span.in_scope(|| info!("Sending commitment for this round"));
        let (nonces, commitment) = self.commit(&synthetics_to_sign);
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

        // TODO: don't stop after the first N committers if any of them are not fully committed
        if commitments.len() < (*self.key.min_signers() as usize) {
            // still collecting
            return Ok(());
        }

        // Map<synthetic, Map<Identifier, SigningCommitments>>
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

        round_span.in_scope(|| {
            info!(
                ?recipients,
                "Collected enough commitments, time to collect signatures"
            )
        });

        let price_data = price_data.clone();
        let mut packages = BTreeMap::new();
        // Request signatures from all the committees
        for entry in &price_data {
            let synthetic = &entry.data.synthetic;
            let message = serialize(&entry.data);
            let Some(commitments) = commitment_maps.remove(synthetic) else {
                continue;
            };
            if commitments.len() < (*self.key.min_signers() as usize) {
                // Don't have enough commitments for this particular package.
                continue;
            }
            let package = SigningPackage::new(commitments, &message);
            packages.insert(synthetic.clone(), package);
        }
        let wire_packages: BTreeMap<String, CborSigningPackage> = packages
            .iter()
            .map(|(synthetic, p)| (synthetic.clone(), p.clone().into()))
            .collect();
        let send_to_recipient_tasks = recipients.into_iter().map(|recipient| {
            let message = SignerMessage {
                round: round.clone(),
                data: SignerMessageData::RequestSignature(SignatureRequest {
                    packages: wire_packages.clone(),
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
        });

        Ok(())
    }

    async fn send_signature(&mut self, round: String, request: SignatureRequest) -> Result<()> {
        let packages: BTreeMap<String, SigningPackage> = request
            .packages
            .into_iter()
            .map(|(synthetic, i)| (synthetic, i.into()))
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
            .do_send_signature(round, &packages)
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
        packages: &BTreeMap<String, SigningPackage>,
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

        let leader_feed: Result<Vec<PriceFeed>> = packages
            .values()
            .map(|p| deserialize(p.message()).context("could not decode price_feed"))
            .collect();
        let my_feed: Vec<PriceFeed> = price_data.iter().map(|entry| entry.data.clone()).collect();

        // Find the set which we are OK with signing
        let mut nonces_to_sign = nonces.clone();
        for (synthetic, result) in choose_feeds_to_sign(&leader_feed?, &my_feed) {
            if let ComparisonResult::DoNotSign(reason) = result {
                warn!(
                    synthetic,
                    error = reason,
                    "Not signing this synthetic in this round"
                );
                nonces_to_sign.remove(synthetic);
            }
        }

        // Send our signature back to the leader
        let signature = self.sign(packages, &nonces_to_sign)?;
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

    async fn process_signature(&mut self, round: String, signature: Signature) -> Result<()> {
        let SignerState::Leader(LeaderState::CollectingSignatures {
            round: curr_round,
            round_span,
            price_data,
            signatures,
            packages,
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

        if signatures.len() < (*self.key.min_signers() as usize) {
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

        let payload_entries = price_data
            .iter()
            .filter_map(|entry| {
                // If we don't have a signature in the map, we're not signing this price feed
                let signature = signatures.remove(&entry.data.synthetic)?;
                Some(SignedEntry {
                    price: entry.price.to_f64().expect("Could not convert decimal"),
                    data: SignedPriceFeed {
                        data: entry.data.clone(),
                        signature,
                    },
                })
            })
            .collect();

        let payload = SignedEntries {
            timestamp: SystemTime::now(),
            entries: payload_entries,
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
        self.signed_entries_sink
            .send((publisher, payload))
            .await
            .context("Could not publish signed result")?;

        Ok(())
    }

    fn commit(&self, synthetics: &[&str]) -> (BTreeMap<String, SigningNonces>, Commitment) {
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
        };
        (nonces, commitment)
    }

    fn sign(
        &self,
        packages: &BTreeMap<String, SigningPackage>,
        nonces: &BTreeMap<String, SigningNonces>,
    ) -> Result<Signature> {
        let mut signatures = BTreeMap::new();
        for (synthetic, nonce) in nonces {
            if let Some(package) = packages.get(synthetic) {
                let signature = round2::sign(package, nonce, &self.key)?;
                signatures.insert(synthetic.clone(), signature.into());
            };
        }
        Ok(Signature {
            identifier: (*self.key.identifier()).into(),
            signatures,
        })
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use frost_ed25519::keys::{self, IdentifierList, KeyPackage};
    use num_bigint::BigUint;
    use rand::thread_rng;
    use rust_decimal::{prelude::FromPrimitive, Decimal};
    use tokio::sync::{mpsc, watch};
    use tracing::{Instrument, Span};

    use crate::{
        network::{NetworkReceiver, NodeId, TestNetwork},
        price_feed::{PriceFeed, PriceFeedEntry, SignedEntries, Validity},
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
        prices: Vec<Vec<PriceFeedEntry>>,
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
        signers[0].process(SignerEvent::RoundStarted).await;
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

    fn price_feed_entry(
        synthetic: &str,
        price: f64,
        collateral_names: &[&str],
        collateral_prices: &[u64],
        denominator: u64,
    ) -> PriceFeedEntry {
        PriceFeedEntry {
            price: Decimal::from_f64(price).unwrap(),
            data: PriceFeed {
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

    #[tokio::test]
    async fn should_sign_payload_with_quorum_of_two() -> Result<()> {
        let price_data = vec![
            price_feed_entry("ADA", 3.50, &["A", "B"], &[3, 4], 1),
            price_feed_entry("BTN", 1.21, &["A", "B"], &[5, 8], 1),
        ];

        let prices = vec![price_data.clone(); 3];
        let (mut signers, mut network, mut payload_source) = construct_signers(2, prices).await?;

        // set everyone's roles
        let leader_id = assign_roles(&mut signers).await;

        // Start the round
        signers[0].process(SignerEvent::RoundStarted).await;

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
        assert_eq!(signed_entries.entries.len(), 2);

        Ok(())
    }

    #[tokio::test]
    async fn should_sign_payload_with_quorum_of_three() -> Result<()> {
        let price_data = vec![
            price_feed_entry("ADA", 3.50, &["A", "B"], &[3, 4], 1),
            price_feed_entry("BTN", 1.21, &["A", "B"], &[5, 8], 1),
        ];

        let (mut signers, mut network, mut payload_source) =
            construct_signers(3, vec![price_data; 4]).await?;
        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        let signed_entries = assert_round_complete(&mut payload_source)?;
        assert_eq!(signed_entries.entries.len(), 2);

        Ok(())
    }

    #[tokio::test]
    async fn should_sign_close_enough_collateral_prices() -> Result<()> {
        let leader_prices = vec![price_feed_entry("SYNTH", 3.50, &["A"], &[2], 1)];
        let follower_prices = vec![price_feed_entry("SYNTH", 3.50, &["A"], &[199], 100)];

        let (mut signers, mut network, mut payload_source) =
            construct_signers(2, vec![leader_prices, follower_prices]).await?;
        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        let signed_entries = assert_round_complete(&mut payload_source)?;
        assert_eq!(signed_entries.entries.len(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn should_not_sign_distant_collateral_prices() -> Result<()> {
        let leader_prices = vec![price_feed_entry("SYNTH", 3.50, &["A"], &[2], 1)];
        let follower_prices = vec![price_feed_entry("SYNTH", 3.50, &["A"], &[3], 2)];

        let (mut signers, mut network, mut payload_source) =
            construct_signers(2, vec![leader_prices, follower_prices]).await?;
        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        let signed_entries = assert_round_complete(&mut payload_source)?;
        assert_eq!(signed_entries.entries.len(), 0);

        Ok(())
    }
}
