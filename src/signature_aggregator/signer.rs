use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use frost_ed25519::{
    aggregate,
    keys::{KeyPackage, PublicKeyPackage},
    round1::{self, SigningCommitments, SigningNonces},
    round2::{self, SignatureShare},
    Identifier, SigningPackage,
};
use rand::thread_rng;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc::Sender, watch::Receiver};
use uuid::Uuid;

use crate::{
    price_feed::{
        deserialize, serialize, IntervalBound, IntervalBoundType, PriceFeed, PriceFeedEntry,
        SignedPriceFeed, SignedPriceFeedEntry, Validity,
    },
    raft::RaftLeader,
};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SignerMessage {
    pub round: String,
    pub data: SignerMessageData,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum SignerMessageData {
    RequestCommitment,
    Commit(Commitment),
    RequestSignature(Vec<SigningPackage>),
    Sign(Signature),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Commitment {
    pub from: String,
    pub identifier: Identifier,
    pub commitments: Vec<SigningCommitments>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Signature {
    pub from: String,
    pub identifier: Identifier,
    pub signatures: Vec<SignatureShare>,
}

pub enum SignerEvent {
    RoundStarted,
    Message(SignerMessage),
    LeaderChanged(RaftLeader),
}

enum LeaderState {
    Ready,
    CollectingCommitments {
        round: String,
        price_data: Vec<PriceFeedEntry>,
        commitments: Vec<Commitment>,
        my_nonces: Vec<SigningNonces>,
    },
    CollectingSignatures {
        round: String,
        price_data: Vec<PriceFeedEntry>,
        signatures: Vec<Signature>,
        packages: Vec<SigningPackage>,
    },
}
enum FollowerState {
    Ready,
    Committed {
        round: String,
        nonces: Vec<SigningNonces>,
    },
}
enum SignerState {
    Leader(LeaderState),
    Follower(String, FollowerState),
    Unknown,
}

pub struct OutgoingMessage {
    pub to: Option<String>,
    pub message: SignerMessage,
}

pub struct Signer {
    id: String,
    key: KeyPackage,
    public_key: PublicKeyPackage,
    price_source: Receiver<Vec<PriceFeedEntry>>,
    message_sink: Sender<OutgoingMessage>,
    signed_price_sink: Sender<Vec<SignedPriceFeedEntry>>,
    state: SignerState,
}

impl Signer {
    pub fn new(
        id: String,
        key: KeyPackage,
        public_key: PublicKeyPackage,
        price_source: Receiver<Vec<PriceFeedEntry>>,
        message_sink: Sender<OutgoingMessage>,
        signed_price_sink: Sender<Vec<SignedPriceFeedEntry>>,
    ) -> Self {
        Self {
            id,
            key,
            public_key,
            price_source,
            message_sink,
            signed_price_sink,
            state: SignerState::Unknown,
        }
    }

    pub async fn process(&mut self, event: SignerEvent) -> Result<()> {
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
            SignerEvent::Message(SignerMessage { round, data }) => match data {
                SignerMessageData::RequestCommitment => {
                    self.send_commitment(round).await?;
                }
                SignerMessageData::Commit(commitment) => {
                    self.process_commitment(round, commitment).await?;
                }
                SignerMessageData::RequestSignature(packages) => {
                    self.send_signature(round, packages).await?;
                }
                SignerMessageData::Sign(signature) => {
                    self.process_signature(round, signature).await?;
                }
            },
        }
        Ok(())
    }

    async fn request_commitments(&mut self) -> Result<()> {
        let round = Uuid::new_v4().to_string();

        let price_data = self.price_source.borrow().clone();

        // request other folks commit
        self.message_sink
            .send(OutgoingMessage {
                to: None, // broadcast this
                message: SignerMessage {
                    round: round.clone(),
                    data: SignerMessageData::RequestCommitment,
                },
            })
            .await
            .context("Failed to request commitments")?;

        // And commit ourself
        let (my_nonces, commitment) = self.commit(price_data.len());
        self.state = SignerState::Leader(LeaderState::CollectingCommitments {
            round,
            price_data,
            commitments: vec![commitment],
            my_nonces,
        });

        Ok(())
    }

    async fn send_commitment(&mut self, round: String) -> Result<()> {
        let SignerState::Follower(leader_id, _) = &self.state else {
            // we're not a follower ATM
            return Ok(());
        };
        let leader_id = leader_id.clone();

        // Send our commitment
        let commitment_count = self.price_source.borrow().len();
        let (nonces, commitment) = self.commit(commitment_count);
        self.message_sink
            .send(OutgoingMessage {
                to: Some(leader_id.clone()),
                message: SignerMessage {
                    round: round.clone(),
                    data: SignerMessageData::Commit(commitment),
                },
            })
            .await
            .context("Failed to commit")?;

        // And wait to be asked to sign
        self.state = SignerState::Follower(
            leader_id.clone(),
            FollowerState::Committed { round, nonces },
        );

        Ok(())
    }

    async fn process_commitment(&mut self, round: String, commitment: Commitment) -> Result<()> {
        let SignerState::Leader(LeaderState::CollectingCommitments {
            round: curr_round,
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

        if commitments
            .iter()
            .any(|c| c.identifier == commitment.identifier)
        {
            // we saw this one
            return Ok(());
        }
        commitments.push(commitment);

        if commitments.len() < (*self.key.min_signers() as usize) {
            // still collecting
            return Ok(());
        }

        let mut commitment_maps = vec![BTreeMap::new(); price_data.len()];
        let mut recipients = vec![];
        // time to transition to signing
        for commitment in commitments {
            if commitment.from != self.id {
                recipients.push(commitment.from.clone());
            }
            for (idx, c) in commitment.commitments.iter().enumerate() {
                commitment_maps[idx].insert(commitment.identifier, *c);
            }
        }

        let price_data = price_data.clone();
        // Request signatures from all the committees
        let messages: Vec<Vec<u8>> = price_data.iter().map(|e| serialize(&e.data)).collect();
        let packages: Vec<SigningPackage> = commitment_maps
            .into_iter()
            .zip(messages)
            .map(|(c, m)| SigningPackage::new(c, &m))
            .collect();
        for recipient in recipients {
            self.message_sink
                .send(OutgoingMessage {
                    to: Some(recipient.clone()),
                    message: SignerMessage {
                        round: round.clone(),
                        data: SignerMessageData::RequestSignature(packages.clone()),
                    },
                })
                .await
                .context(format!("Failed to request signature from {}", recipient))?;
        }

        // And be sure to sign the message ourself
        let nonces = my_nonces.clone();
        let my_signature = self.sign(&packages, &nonces)?;

        self.state = SignerState::Leader(LeaderState::CollectingSignatures {
            round,
            price_data,
            signatures: vec![my_signature],
            packages,
        });

        Ok(())
    }

    async fn send_signature(&mut self, round: String, packages: Vec<SigningPackage>) -> Result<()> {
        let SignerState::Follower(
            leader_id,
            FollowerState::Committed {
                round: curr_round,
                nonces,
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

        let price_data = self.price_source.borrow().clone();

        // Make sure we are OK with signing this
        if packages.len() != price_data.len() {
            return Err(anyhow!(
                "Mismatched price feed count. Is this server misconfigured?"
            ));
        }
        for (package, my_feed) in packages.iter().zip(price_data.iter()) {
            let leader_feed =
                deserialize(package.message()).context("could not decode price_feed")?;
            if let Err(mismatch) = should_sign(&leader_feed, &my_feed.data) {
                return Err(anyhow!("Not signing payload: {}", mismatch));
            }
        }

        // Send our signature back to the leader
        let signature = self.sign(&packages, nonces)?;
        self.message_sink
            .send(OutgoingMessage {
                to: Some(leader_id.clone()),
                message: SignerMessage {
                    round,
                    data: SignerMessageData::Sign(signature),
                },
            })
            .await
            .context("Failed to send signature to leader")?;

        // and vibe until we get back into the signature flow
        self.state = SignerState::Follower(leader_id.clone(), FollowerState::Ready);

        Ok(())
    }

    async fn process_signature(&mut self, round: String, signature: Signature) -> Result<()> {
        let SignerState::Leader(LeaderState::CollectingSignatures {
            round: curr_round,
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

        // We've finally collected all the signatures we need! Now just aggregate them
        let mut signature_maps = vec![BTreeMap::new(); price_data.len()];
        for signature in signatures {
            for (index, s) in signature.signatures.iter().enumerate() {
                signature_maps[index].insert(signature.identifier, *s);
            }
        }

        let mut signatures = vec![];
        for (package, sigs) in packages.iter().zip(signature_maps) {
            let signature = aggregate(package, &sigs, &self.public_key)?;
            signatures.push(signature);
        }

        let payload = price_data
            .iter()
            .zip(signatures)
            .map(|(price, sig)| SignedPriceFeedEntry {
                price: price.price,
                data: SignedPriceFeed {
                    data: price.data.clone(),
                    signature: sig.serialize().into(),
                },
            })
            .collect();

        self.signed_price_sink
            .send(payload)
            .await
            .context("Could not publish signed result")?;

        // And now that we've signed something, wait for the next round
        self.state = SignerState::Leader(LeaderState::Ready);

        Ok(())
    }

    fn commit(&self, commitment_count: usize) -> (Vec<SigningNonces>, Commitment) {
        let mut nonces = vec![];
        let mut commitments = vec![];
        for _ in 0..commitment_count {
            let (nonce, commitment) = round1::commit(self.key.signing_share(), &mut thread_rng());
            nonces.push(nonce);
            commitments.push(commitment);
        }
        let commitment = Commitment {
            from: self.id.clone(),
            identifier: *self.key.identifier(),
            commitments,
        };
        (nonces, commitment)
    }

    fn sign(&self, packages: &[SigningPackage], nonces: &[SigningNonces]) -> Result<Signature> {
        let mut signatures = vec![];
        for (package, nonce) in packages.iter().zip(nonces) {
            let signature = round2::sign(package, nonce, &self.key)?;
            signatures.push(signature);
        }
        Ok(Signature {
            from: self.id.clone(),
            identifier: *self.key.identifier(),
            signatures,
        })
    }
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
    use anyhow::{anyhow, Result};
    use frost_ed25519::keys::{self, IdentifierList, KeyPackage};
    use num_bigint::BigUint;
    use rand::thread_rng;
    use rust_decimal::{prelude::FromPrimitive, Decimal};
    use tokio::sync::{mpsc, watch};

    use crate::{
        price_feed::{IntervalBound, PriceFeed, PriceFeedEntry, SignedPriceFeedEntry, Validity},
        raft::RaftLeader,
        signature_aggregator::signer::{OutgoingMessage, SignerMessage, SignerMessageData},
    };

    use super::{Signer, SignerEvent};

    async fn construct_signers(
        min: u16,
        prices: Vec<Vec<PriceFeedEntry>>,
    ) -> Result<(
        Vec<Signer>,
        mpsc::Receiver<OutgoingMessage>,
        mpsc::Receiver<Vec<SignedPriceFeedEntry>>,
    )> {
        let mut rng = thread_rng();
        let (shares, public_key) = keys::generate_with_dealer(
            prices.len() as u16,
            min,
            IdentifierList::Default,
            &mut rng,
        )?;

        let (message_sink, message_source) = mpsc::channel(10);
        let (payload_sink, payload_source) = mpsc::channel(10);

        let signers: Vec<Signer> = shares
            .into_iter()
            .zip(prices.into_iter())
            .enumerate()
            .map(|(idx, ((_, share), price_data))| {
                let id = idx.to_string();
                let key = KeyPackage::try_from(share).unwrap();
                let (_, price_source) = watch::channel(price_data);
                Signer::new(
                    id,
                    key,
                    public_key.clone(),
                    price_source,
                    message_sink.clone(),
                    payload_sink.clone(),
                )
            })
            .collect();

        Ok((signers, message_source, payload_source))
    }

    // make the first signer the leader, and other signers followers.
    // return the leader's id
    async fn assign_roles(signers: &mut [Signer]) -> Result<String> {
        let leader_id = signers[0].id.clone();
        signers[0]
            .process(SignerEvent::LeaderChanged(RaftLeader::Myself))
            .await?;
        for signer in signers.iter_mut().skip(1) {
            signer
                .process(SignerEvent::LeaderChanged(RaftLeader::Other(
                    leader_id.clone(),
                )))
                .await?;
        }
        Ok(leader_id)
    }

    async fn run_round(
        signers: &mut [Signer],
        message_source: &mut mpsc::Receiver<OutgoingMessage>,
    ) -> Result<()> {
        signers[0].process(SignerEvent::RoundStarted).await?;
        while let Ok(message) = message_source.try_recv() {
            let to = message.to;
            let message = message.message;
            let receivers = signers
                .iter_mut()
                .filter(|s| to.is_none() || to.as_ref().unwrap() == &s.id);
            for receiver in receivers {
                receiver
                    .process(SignerEvent::Message(message.clone()))
                    .await?;
            }
        }
        Ok(())
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
                    .into_iter()
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
        let (mut signers, mut message_source, mut payload_source) =
            construct_signers(2, prices).await?;

        // set everyone's roles
        let leader_id = assign_roles(&mut signers).await?;

        // Start the round
        signers[0].process(SignerEvent::RoundStarted).await?;

        // Leader should have broadcast a request to all followers
        let message = message_source.recv().await;
        let Some(OutgoingMessage {
            to: None,
            message:
                SignerMessage {
                    round,
                    data: SignerMessageData::RequestCommitment,
                },
        }) = message
        else {
            return Err(anyhow!("Unexpected response"));
        };

        // Make a single follower commit
        signers[1]
            .process(SignerEvent::Message(SignerMessage {
                round: round.clone(),
                data: SignerMessageData::RequestCommitment,
            }))
            .await?;

        // Follower should have sent a commitment to the leader
        let message = message_source.recv().await;
        let Some(OutgoingMessage {
            to: sent_to,
            message:
                SignerMessage {
                    data: SignerMessageData::Commit(commitment),
                    ..
                },
        }) = message
        else {
            return Err(anyhow!("Unexpected response"));
        };
        assert_eq!(sent_to, Some(leader_id.clone()));

        // Send that commitment to the leader
        signers[0]
            .process(SignerEvent::Message(SignerMessage {
                round: round.clone(),
                data: SignerMessageData::Commit(commitment),
            }))
            .await?;

        // Leader should have sent a signing request to that follower
        let message = message_source.recv().await;
        let Some(OutgoingMessage {
            to: sent_to,
            message:
                SignerMessage {
                    data: SignerMessageData::RequestSignature(packages),
                    ..
                },
        }) = message
        else {
            return Err(anyhow!("Unexpected response"));
        };
        assert_eq!(sent_to, Some(signers[1].id.clone()));

        // Send that request to the follower
        signers[1]
            .process(SignerEvent::Message(SignerMessage {
                round: round.clone(),
                data: SignerMessageData::RequestSignature(packages),
            }))
            .await?;

        // Follower should have sent a signature to the leader
        let message = message_source.recv().await;
        let Some(OutgoingMessage {
            to: sent_to,
            message:
                SignerMessage {
                    data: SignerMessageData::Sign(signature),
                    ..
                },
        }) = message
        else {
            return Err(anyhow!("Unexpected response"));
        };
        assert_eq!(sent_to, Some(leader_id));

        // Send that signature to the leader
        signers[0]
            .process(SignerEvent::Message(SignerMessage {
                round: round.clone(),
                data: SignerMessageData::Sign(signature),
            }))
            .await?;

        // leader should have published results
        assert!(payload_source.recv().await.is_some());

        Ok(())
    }

    #[tokio::test]
    async fn should_sign_close_enough_collateral_prices() -> Result<()> {
        let leader_prices = vec![price_feed_entry("TOKEN", 3.50, &[2], 1)];
        let follower_prices = vec![price_feed_entry("TOKEN", 3.50, &[199], 100)];

        let (mut signers, mut message_source, mut payload_source) =
            construct_signers(2, vec![leader_prices, follower_prices]).await?;
        assign_roles(&mut signers).await?;
        run_round(&mut signers, &mut message_source).await?;

        assert!(payload_source.recv().await.is_some());

        Ok(())
    }

    #[tokio::test]
    async fn should_not_sign_distant_collateral_prices() -> Result<()> {
        let leader_prices = vec![price_feed_entry("TOKEN", 3.50, &[2], 1)];
        let follower_prices = vec![price_feed_entry("TOKEN", 3.50, &[3], 2)];

        let (mut signers, mut message_source, mut payload_source) =
            construct_signers(2, vec![leader_prices, follower_prices]).await?;
        assign_roles(&mut signers).await?;
        let err = run_round(&mut signers, &mut message_source)
            .await
            .expect_err("round should have failed");
        assert_eq!(
            err.to_string(),
            "Not signing payload: collateral prices are too distant: leader has 2/1, we have 3/2"
        );

        assert!(payload_source.try_recv().is_err());

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

        let (mut signers, mut message_source, mut payload_source) =
            construct_signers(2, vec![vec![leader_price], vec![follower_price]]).await?;
        assign_roles(&mut signers).await?;
        run_round(&mut signers, &mut message_source).await?;

        assert!(payload_source.recv().await.is_some());

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

        let (mut signers, mut message_source, mut payload_source) =
            construct_signers(2, vec![vec![leader_price], vec![follower_price]]).await?;
        assign_roles(&mut signers).await?;
        let err = run_round(&mut signers, &mut message_source)
            .await
            .expect_err("round should have failed");
        assert!(err
            .to_string()
            .starts_with("Not signing payload: mismatched validity"));

        assert!(payload_source.try_recv().is_err());

        Ok(())
    }
}
