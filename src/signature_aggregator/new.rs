use std::collections::BTreeMap;

use anyhow::{Context, Result};
use frost_ed25519::{
    aggregate,
    keys::{KeyPackage, PublicKeyPackage},
    round1::{self, SigningCommitments, SigningNonces},
    round2::{self, SignatureShare},
    Identifier, SigningPackage,
};
use rand::{rngs::ThreadRng, thread_rng};
use rust_decimal::Decimal;
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

use crate::{
    price_feed::{PriceFeed, PriceFeedEntry},
    raft::RaftLeader,
};

pub struct SignerMessage {
    pub round: String,
    pub data: SignerMessageData,
}

pub enum SignerMessageData {
    RequestCommitment,
    Commit(Commitment),
    RequestSignature(Vec<SigningPackage>),
    Sign(Signature),
}

pub struct Commitment {
    pub from: String,
    pub identifier: Identifier,
    pub commitments: Vec<SigningCommitments>,
}

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
        commitments: Vec<Commitment>,
        my_nonces: Vec<SigningNonces>,
    },
    CollectingSignatures {
        signatures: Vec<Signature>,
        packages: Vec<SigningPackage>,
    },
}
enum FollowerState {
    Ready,
    Committed { nonces: Vec<SigningNonces> },
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

pub struct SignedPayloadEntry {
    pub synthetic: String,
    pub price: Decimal,
    pub price_feed: PriceFeed,
    pub signature: Vec<u8>,
}

const QUORUM_COUNT: usize = 2;
pub struct Signer {
    id: String,
    identifier: Identifier,
    key: KeyPackage,
    public_key: PublicKeyPackage,
    price_data: Vec<PriceFeedEntry>,
    message_sink: Sender<OutgoingMessage>,
    payload_sink: Sender<Vec<SignedPayloadEntry>>,
    state: SignerState,
    current_round: Option<String>,
    rng: ThreadRng,
}

impl Signer {
    pub fn new(
        id: String,
        identifier: Identifier,
        key: KeyPackage,
        public_key: PublicKeyPackage,
        price_data: Vec<PriceFeedEntry>, // TODO: this should be dynamic
        message_sink: Sender<OutgoingMessage>,
        payload_sink: Sender<Vec<SignedPayloadEntry>>,
    ) -> Self {
        Self {
            id,
            identifier,
            key,
            public_key,
            price_data,
            message_sink,
            payload_sink,
            state: SignerState::Unknown,
            current_round: None,
            rng: thread_rng(),
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
            SignerEvent::Message(SignerMessage { round, data }) => {
                if let SignerMessageData::RequestCommitment = data {
                    if let SignerState::Follower(_, _) = self.state {
                        self.current_round = Some(round);
                    }
                } else if !self.current_round.as_ref().is_some_and(|r| *r == round) {
                    // message from a different round
                    return Ok(());
                }
                match data {
                    SignerMessageData::RequestCommitment => {
                        self.send_commitment().await?;
                    }
                    SignerMessageData::Commit(commitment) => {
                        self.process_commitment(commitment).await?;
                    }
                    SignerMessageData::RequestSignature(packages) => {
                        self.send_signature(packages).await?;
                    }
                    SignerMessageData::Sign(signature) => {
                        self.process_signature(signature).await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn request_commitments(&mut self) -> Result<()> {
        let round = Uuid::new_v4().to_string();
        self.current_round = Some(round.clone());

        // request other folks commit
        self.message_sink
            .send(OutgoingMessage {
                to: None, // broadcast this
                message: SignerMessage {
                    round,
                    data: SignerMessageData::RequestCommitment,
                },
            })
            .await
            .context("Failed to request commitments")?;

        // And commit ourself
        let (my_nonces, commitment) = self.commit();
        self.state = SignerState::Leader(LeaderState::CollectingCommitments {
            commitments: vec![commitment],
            my_nonces,
        });

        Ok(())
    }

    async fn send_commitment(&mut self) -> Result<()> {
        let SignerState::Follower(leader_id, _) = &self.state else {
            // we're not a follower ATM
            return Ok(());
        };
        let leader_id = leader_id.clone();
        let Some(round) = &self.current_round else {
            // we're not mid-round
            return Ok(());
        };
        let round = round.clone();

        // Send our commitment
        let (nonces, commitment) = self.commit();
        self.message_sink
            .send(OutgoingMessage {
                to: Some(leader_id.clone()),
                message: SignerMessage {
                    round,
                    data: SignerMessageData::Commit(commitment),
                },
            })
            .await
            .context("Failed to commit")?;

        // And wait to be asked to sign
        self.state = SignerState::Follower(leader_id.clone(), FollowerState::Committed { nonces });

        Ok(())
    }

    async fn process_commitment(&mut self, commitment: Commitment) -> Result<()> {
        let SignerState::Leader(LeaderState::CollectingCommitments {
            commitments,
            my_nonces,
        }) = &mut self.state
        else {
            // we're not the leader ATM, or not waiting for commitments
            return Ok(());
        };
        let Some(round) = &self.current_round else {
            // we're not mid-round
            return Ok(());
        };
        let round = round.clone();

        if commitments
            .iter()
            .any(|c| c.identifier == commitment.identifier)
        {
            // we saw this one
            return Ok(());
        }
        commitments.push(commitment);

        if commitments.len() < QUORUM_COUNT {
            // still collecting
            return Ok(());
        }

        let mut commitment_maps = vec![BTreeMap::new(); self.price_data.len()];
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

        // Request signatures from all the committees
        // TODO: actually serialize the message
        let messages: Vec<&str> = self.price_data.iter().map(|_| "Hello, world!").collect();
        let packages: Vec<SigningPackage> = commitment_maps
            .into_iter()
            .zip(messages)
            .map(|(c, m)| SigningPackage::new(c, m.as_bytes()))
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

        // And be sure ot sign the message ourself
        let nonces = my_nonces.clone();
        let my_signature = self.sign(&packages, &nonces)?;

        self.state = SignerState::Leader(LeaderState::CollectingSignatures {
            signatures: vec![my_signature],
            packages,
        });

        Ok(())
    }

    async fn send_signature(&mut self, packages: Vec<SigningPackage>) -> Result<()> {
        let SignerState::Follower(leader_id, FollowerState::Committed { nonces }) = &self.state
        else {
            // We're not a follower, or we aren't waiting to sign something
            return Ok(());
        };
        let Some(round) = self.current_round.take() else {
            // we're not mid-round
            return Ok(());
        };

        // TODO: validate what we sign

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

    async fn process_signature(&mut self, signature: Signature) -> Result<()> {
        let SignerState::Leader(LeaderState::CollectingSignatures {
            signatures,
            packages,
        }) = &mut self.state
        else {
            // We're not a leader, or we're not waiting for signatures
            return Ok(());
        };

        if signatures
            .iter()
            .any(|s| s.identifier == signature.identifier)
        {
            // we saw this one
            return Ok(());
        }
        signatures.push(signature);

        if signatures.len() < QUORUM_COUNT {
            // still collecting
            return Ok(());
        }

        // We've finally collected all the signatures we need! Now just aggregate them
        let mut signature_maps = vec![BTreeMap::new(); self.price_data.len()];
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

        let payload = self
            .price_data
            .iter()
            .zip(signatures)
            .map(|(price, sig)| SignedPayloadEntry {
                synthetic: price.data.synthetic.clone(),
                price: price.price,
                price_feed: price.data.clone(),
                signature: sig.serialize().into(),
            })
            .collect();

        self.payload_sink
            .send(payload)
            .await
            .context("Could not publish signed result")?;

        // And now that we've signed something, wait for the next round
        self.state = SignerState::Leader(LeaderState::Ready);

        Ok(())
    }

    fn commit(&mut self) -> (Vec<SigningNonces>, Commitment) {
        let mut nonces = vec![];
        let mut commitments = vec![];
        for _ in 0..self.price_data.len() {
            let (nonce, commitment) = round1::commit(self.key.signing_share(), &mut self.rng);
            nonces.push(nonce);
            commitments.push(commitment);
        }
        let commitment = Commitment {
            from: self.id.clone(),
            identifier: self.identifier,
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
            identifier: self.identifier,
            signatures,
        })
    }
}

#[cfg(test)]
mod tests {
    use anyhow::{anyhow, Result};
    use frost_ed25519::keys::{self, IdentifierList, KeyPackage};
    use num_bigint::BigUint;
    use rand::thread_rng;
    use rust_decimal::Decimal;
    use tokio::sync::mpsc;

    use crate::{
        price_feed::{PriceFeed, PriceFeedEntry, Validity},
        raft::RaftLeader,
        signature_aggregator::new::{OutgoingMessage, SignerMessage, SignerMessageData},
    };

    use super::{Signer, SignerEvent};

    #[tokio::test]
    async fn should_sign_payload_with_quorum_of_two() -> Result<()> {
        let price_data = vec![
            PriceFeedEntry {
                price: Decimal::new(350, 2),
                data: PriceFeed {
                    collateral_prices: vec![BigUint::from(3u8), BigUint::from(4u8)],
                    synthetic: "ADA".into(),
                    denominator: BigUint::from(1u8),
                    validity: Validity::default(),
                },
            },
            PriceFeedEntry {
                price: Decimal::new(121, 2),
                data: PriceFeed {
                    collateral_prices: vec![BigUint::from(5u8), BigUint::from(8u8)],
                    synthetic: "BTN".into(),
                    denominator: BigUint::from(1u8),
                    validity: Validity::default(),
                },
            },
        ];

        let mut rng = thread_rng();
        let (shares, public_key) =
            keys::generate_with_dealer(3, 2, IdentifierList::Default, &mut rng)?;

        let (message_sink, mut message_source) = mpsc::channel(10);
        let (payload_sink, mut payload_source) = mpsc::channel(10);

        let mut signers: Vec<Signer> = shares
            .into_iter()
            .enumerate()
            .map(|(idx, (identifier, share))| {
                let id = idx.to_string();
                let key = KeyPackage::try_from(share).unwrap();
                Signer::new(
                    id,
                    identifier,
                    key,
                    public_key.clone(),
                    price_data.clone(),
                    message_sink.clone(),
                    payload_sink.clone(),
                )
            })
            .collect();

        // set everyone's roles
        let leader_id = signers[0].id.clone();
        signers[0]
            .process(SignerEvent::LeaderChanged(RaftLeader::Myself))
            .await?;
        signers[1]
            .process(SignerEvent::LeaderChanged(RaftLeader::Other(
                leader_id.clone(),
            )))
            .await?;
        signers[2]
            .process(SignerEvent::LeaderChanged(RaftLeader::Other(
                leader_id.clone(),
            )))
            .await?;

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
}
