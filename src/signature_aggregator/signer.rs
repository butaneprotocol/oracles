use std::{
    collections::BTreeMap,
    fmt::{self, Display},
    time::SystemTime,
};

use anyhow::{anyhow, Context, Result};
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
        deserialize, serialize, IntervalBound, IntervalBoundType, PriceFeed, PriceFeedEntry,
        SignedEntries, SignedEntry, SignedPriceFeed, Validity,
    },
    raft::RaftLeader,
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
    price_feed: Vec<PriceFeed>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct Commitment {
    #[n(0)]
    pub identifier: CborIdentifier,
    #[n(1)]
    pub commitments: Vec<CborSigningCommitments>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct SignatureRequest {
    #[n(0)]
    packages: Vec<CborSigningPackage>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct Signature {
    #[n(0)]
    pub identifier: CborIdentifier,
    #[n(1)]
    pub signatures: Vec<CborSignatureShare>,
}

#[derive(Clone)]
pub enum SignerEvent {
    RoundStarted,
    Message(NodeId, SignerMessage),
    LeaderChanged(RaftLeader),
}
impl Display for SignerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RoundStarted => f.write_str("RoundStarted"),
            Self::Message(from, message) => {
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
        my_nonces: Vec<SigningNonces>,
    },
    CollectingSignatures {
        round: String,
        round_span: Span,
        price_data: Vec<PriceFeedEntry>,
        signatures: Vec<Signature>,
        packages: Vec<SigningPackage>,
    },
}
impl Display for LeaderState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready => f.write_str("{role=Leader,state=Ready}"),
            Self::CollectingCommitments {
                round, commitments, ..
            } => {
                let committed: Vec<Identifier> = commitments
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
                let committed: Vec<_> = packages[0].signing_commitments().keys().collect();
                let signed: Vec<_> = signatures.iter().map(|s| s.identifier).collect();
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
        nonces: Vec<SigningNonces>,
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

    #[instrument(skip_all, fields(round = self.state.round(), state = %self.state))]
    pub async fn process(&mut self, event: SignerEvent) {
        info!("Processing event: {}", event);
        match self.do_process(event.clone()).await {
            Ok(()) => {
                info!("Processed event: {}", event)
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
            SignerEvent::Message(from, SignerMessage { round, data }) => match data {
                SignerMessageData::RequestCommitment(request) => {
                    self.send_commitment(round, request).await?;
                }
                SignerMessageData::Commit(commitment) => {
                    self.process_commitment(round, from, commitment).await?;
                }
                SignerMessageData::RequestSignature(request) => {
                    self.send_signature(round, request).await?;
                }
                SignerMessageData::Sign(signature) => {
                    self.process_signature(round, signature).await?;
                }
                SignerMessageData::Publish(payload) => {
                    self.publish(from, payload).await?;
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
        let (my_nonces, commitment) = self.commit(price_data.len());
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
        if let Err(mismatch) = should_sign_all(&request.price_feed, &my_feed) {
            round_span
                .in_scope(|| warn!(error = mismatch, "Not sending commitment for this round"));
            return Ok(());
        }

        // Send our commitment
        round_span.in_scope(|| info!("Sending commitment for this round"));
        let commitment_count = price_data.len();
        let (nonces, commitment) = self.commit(commitment_count);
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

        if commitments.len() < (*self.key.min_signers() as usize) {
            // still collecting
            return Ok(());
        }

        let mut commitment_maps = vec![BTreeMap::new(); price_data.len()];
        let mut recipients = vec![];
        // time to transition to signing
        for (signer, commitment) in commitments {
            if *signer != self.id {
                recipients.push(signer.clone());
            }
            for (idx, c) in commitment.commitments.iter().enumerate() {
                let comm: SigningCommitments = (*c).into();
                commitment_maps[idx].insert(commitment.identifier.into(), comm);
            }
        }

        round_span.in_scope(|| {
            info!(
                ?recipients,
                "Collected enough commitments, time to collect signatures"
            )
        });

        let price_data = price_data.clone();
        // Request signatures from all the committees
        let messages: Vec<Vec<u8>> = price_data.iter().map(|e| serialize(&e.data)).collect();
        let packages: Vec<SigningPackage> = commitment_maps
            .into_iter()
            .zip(messages)
            .map(|(c, m)| SigningPackage::new(c, &m))
            .collect();
        let wire_packages: Vec<CborSigningPackage> =
            packages.iter().map(|p| p.clone().into()).collect();
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
        let packages: Vec<SigningPackage> =
            request.packages.into_iter().map(|i| i.into()).collect();
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
        packages: &[SigningPackage],
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

        // Make sure we are OK with signing this
        if packages.len() != price_data.len() {
            return Err(anyhow!(
                "Mismatched price feed count. Is this server misconfigured?"
            ));
        }
        let leader_feed: Result<Vec<PriceFeed>> = packages
            .iter()
            .map(|p| deserialize(p.message()).context("could not decode price_feed"))
            .collect();
        let my_feed: Vec<PriceFeed> = price_data.iter().map(|entry| entry.data.clone()).collect();
        if let Err(mismatch) = should_sign_all(&leader_feed?, &my_feed) {
            return Err(anyhow!("Not signing payload: {}", mismatch));
        }

        // Send our signature back to the leader
        let signature = self.sign(packages, nonces)?;
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
        let mut signature_maps = vec![BTreeMap::new(); price_data.len()];
        for signature in signatures {
            for (index, s) in signature.signatures.iter().enumerate() {
                let sig: SignatureShare = (*s).into();
                signature_maps[index].insert(signature.identifier.into(), sig);
            }
        }

        let mut signatures = vec![];
        for (package, sigs) in packages.iter().zip(signature_maps) {
            let signature = aggregate(package, &sigs, &self.public_key)?;
            signatures.push(signature.serialize()?);
        }

        let payload_entries = price_data
            .iter()
            .zip(signatures)
            .map(|(price, signature)| SignedEntry {
                price: price.price.to_f64().expect("Could not convert decimal"),
                data: SignedPriceFeed {
                    data: price.data.clone(),
                    signature,
                },
            })
            .collect();

        let payload = SignedEntries {
            timestamp: SystemTime::now(),
            entries: payload_entries,
        };

        self.signed_entries_sink
            .send((self.id.clone(), payload.clone()))
            .await
            .context("Could not publish signed result")?;

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

    fn commit(&self, commitment_count: usize) -> (Vec<SigningNonces>, Commitment) {
        let mut nonces = vec![];
        let mut commitments = vec![];
        for _ in 0..commitment_count {
            let (nonce, commitment) = round1::commit(self.key.signing_share(), &mut thread_rng());
            nonces.push(nonce);
            commitments.push(commitment.into());
        }
        let commitment = Commitment {
            identifier: (*self.key.identifier()).into(),
            commitments,
        };
        (nonces, commitment)
    }

    fn sign(&self, packages: &[SigningPackage], nonces: &[SigningNonces]) -> Result<Signature> {
        let mut signatures = vec![];
        for (package, nonce) in packages.iter().zip(nonces) {
            let signature = round2::sign(package, nonce, &self.key)?;
            signatures.push(signature.into());
        }
        Ok(Signature {
            identifier: (*self.key.identifier()).into(),
            signatures,
        })
    }
}

fn should_sign_all(leader_feed: &[PriceFeed], my_feed: &[PriceFeed]) -> Result<(), String> {
    if leader_feed.len() != my_feed.len() {
        return Err("Mismatched price feed count. Is this server misconfigured?".into());
    }
    for (leader, mine) in leader_feed.iter().zip(my_feed) {
        should_sign(leader, mine)?;
    }
    Ok(())
}

fn should_sign(leader_feed: &PriceFeed, my_feed: &PriceFeed) -> Result<(), String> {
    if leader_feed.synthetic != my_feed.synthetic {
        return Err(format!(
            "mismatched synthetics: leader has {}, we have {}",
            leader_feed.synthetic, my_feed.synthetic
        ));
    }
    if !is_validity_close_enough(&leader_feed.validity, &my_feed.validity) {
        return Err(format!(
            "mismatched validity: leader has {:?}, we have {:?}",
            leader_feed.validity, my_feed.validity
        ));
    }
    if leader_feed.collateral_prices.len() != my_feed.collateral_prices.len() {
        return Err(format!(
            "wrong number of collateral prices: leader has {}, we have {}",
            leader_feed.collateral_prices.len(),
            my_feed.collateral_prices.len()
        ));
    }
    // If one price is >1% less than the other, they're too distant to trust
    for (leader_price, my_price) in leader_feed
        .collateral_prices
        .iter()
        .zip(my_feed.collateral_prices.iter())
    {
        let leader_value = leader_price * &my_feed.denominator;
        let my_value = my_price * &leader_feed.denominator;
        let max_value = leader_value.clone().max(my_value.clone());
        let min_value = leader_value.min(my_value);
        let difference = &max_value - min_value;
        if difference * 100u32 > max_value {
            return Err(format!(
                "collateral prices are too distant: leader has {}/{}, we have {}/{}",
                leader_price, leader_feed.denominator, my_price, my_feed.denominator
            ));
        }
    }
    Ok(())
}
fn is_validity_close_enough(leader_validity: &Validity, my_validity: &Validity) -> bool {
    are_bounds_close_enough(&leader_validity.lower_bound, &my_validity.lower_bound)
        && are_bounds_close_enough(&leader_validity.upper_bound, &my_validity.upper_bound)
}

fn are_bounds_close_enough(leader_bound: &IntervalBound, my_bound: &IntervalBound) -> bool {
    if let IntervalBoundType::Finite(leader_moment) = leader_bound.bound_type {
        if let IntervalBoundType::Finite(my_moment) = my_bound.bound_type {
            let difference = leader_moment.max(my_moment) - leader_moment.min(my_moment);
            // allow up to 60 seconds difference between bounds
            return difference < 1000 * 60;
        }
    }
    leader_bound == my_bound
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use frost_ed25519::keys::{self, IdentifierList, KeyPackage};
    use num_bigint::BigUint;
    use rand::thread_rng;
    use rust_decimal::{prelude::FromPrimitive, Decimal};
    use tokio::sync::{
        mpsc::{self, error::TryRecvError},
        watch,
    };

    use crate::{
        network::{NetworkReceiver, NodeId, TestNetwork},
        price_feed::{IntervalBound, PriceFeed, PriceFeedEntry, SignedEntries, Validity},
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
                    .process(SignerEvent::Message(message.from, message.data))
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

    fn assert_round_complete(payload_source: &mut mpsc::Receiver<(NodeId, SignedEntries)>) {
        assert!(payload_source.try_recv().is_ok());
    }

    fn assert_round_not_complete(payload_source: &mut mpsc::Receiver<(NodeId, SignedEntries)>) {
        assert!(matches!(
            payload_source.try_recv(),
            Err(TryRecvError::Empty)
        ));
    }

    fn price_feed_entry(
        synthetic: &str,
        price: f64,
        collateral_prices: &[u64],
        denominator: u64,
    ) -> PriceFeedEntry {
        PriceFeedEntry {
            price: Decimal::from_f64(price).unwrap(),
            data: PriceFeed {
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
            price_feed_entry("ADA", 3.50, &[3, 4], 1),
            price_feed_entry("BTN", 1.21, &[5, 8], 1),
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
        assert_round_complete(&mut payload_source);

        Ok(())
    }

    #[tokio::test]
    async fn should_sign_payload_with_quorum_of_three() -> Result<()> {
        let price_data = vec![
            price_feed_entry("ADA", 3.50, &[3, 4], 1),
            price_feed_entry("BTN", 1.21, &[5, 8], 1),
        ];

        let (mut signers, mut network, mut payload_source) =
            construct_signers(3, vec![price_data; 4]).await?;
        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        assert_round_complete(&mut payload_source);

        Ok(())
    }

    #[tokio::test]
    async fn should_sign_close_enough_collateral_prices() -> Result<()> {
        let leader_prices = vec![price_feed_entry("TOKEN", 3.50, &[2], 1)];
        let follower_prices = vec![price_feed_entry("TOKEN", 3.50, &[199], 100)];

        let (mut signers, mut network, mut payload_source) =
            construct_signers(2, vec![leader_prices, follower_prices]).await?;
        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        assert_round_complete(&mut payload_source);

        Ok(())
    }

    #[tokio::test]
    async fn should_not_sign_distant_collateral_prices() -> Result<()> {
        let leader_prices = vec![price_feed_entry("TOKEN", 3.50, &[2], 1)];
        let follower_prices = vec![price_feed_entry("TOKEN", 3.50, &[3], 2)];

        let (mut signers, mut network, mut payload_source) =
            construct_signers(2, vec![leader_prices, follower_prices]).await?;
        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        assert_round_not_complete(&mut payload_source);

        Ok(())
    }

    #[tokio::test]
    async fn should_sign_close_enough_validity() -> Result<()> {
        const TIMESTAMP: u64 = 1712723729359;
        let mut leader_price = price_feed_entry("TOKEN", 3.50, &[1], 1);
        leader_price.data.validity = Validity {
            lower_bound: IntervalBound::unix_timestamp(TIMESTAMP, true),
            upper_bound: IntervalBound::unix_timestamp(TIMESTAMP + 3000000, true),
        };
        let mut follower_price = price_feed_entry("TOKEN", 3.50, &[1], 1);
        follower_price.data.validity = Validity {
            lower_bound: IntervalBound::unix_timestamp(TIMESTAMP + 5000, true),
            upper_bound: IntervalBound::unix_timestamp(TIMESTAMP + 3005000, true),
        };

        let (mut signers, mut network, mut payload_source) =
            construct_signers(2, vec![vec![leader_price], vec![follower_price]]).await?;
        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        assert_round_complete(&mut payload_source);

        Ok(())
    }

    #[tokio::test]
    async fn should_not_sign_distant_validity() -> Result<()> {
        const TIMESTAMP: u64 = 1712723729359;
        let mut leader_price = price_feed_entry("TOKEN", 3.50, &[1], 1);
        leader_price.data.validity = Validity {
            lower_bound: IntervalBound::unix_timestamp(TIMESTAMP, true),
            upper_bound: IntervalBound::unix_timestamp(TIMESTAMP + 3000000, true),
        };
        let mut follower_price = price_feed_entry("TOKEN", 3.50, &[1], 1);
        follower_price.data.validity = Validity {
            lower_bound: IntervalBound::unix_timestamp(TIMESTAMP + 5000000, true),
            upper_bound: IntervalBound::unix_timestamp(TIMESTAMP + 8000000, true),
        };

        let (mut signers, mut network, mut payload_source) =
            construct_signers(2, vec![vec![leader_price], vec![follower_price]]).await?;
        assign_roles(&mut signers).await;
        run_round(&mut signers, &mut network).await;

        assert_round_not_complete(&mut payload_source);

        Ok(())
    }
}
