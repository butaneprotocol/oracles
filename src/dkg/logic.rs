use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use crate::{
    cbor::{CborDkgRound1Package, CborDkgRound2Package},
    network::{NetworkChannel, NetworkSender, NodeId},
};
use anyhow::{anyhow, Result};
use frost_ed25519::{
    keys::{
        dkg::{part1, part2, part3, round1, round2},
        KeyPackage, PublicKeyPackage,
    },
    Identifier,
};
use futures::future::join_all;
use minicbor::{Decode, Encode};
use pallas_crypto::hash::Hasher;
use rand::thread_rng;
use tokio::{join, select, sync::RwLock, time::sleep};
use tracing::{debug, info, trace};

#[derive(Decode, Encode, Clone, Debug)]
pub struct Part1Message {
    #[n(0)]
    pub package: Box<CborDkgRound1Package>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct Part2Message {
    #[n(0)]
    pub session_id: String,
    #[n(1)]
    pub package: Box<CborDkgRound2Package>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct DoneMessage {
    #[n(0)]
    pub session_id: String,
}

#[derive(Decode, Encode, Clone, Debug)]
pub enum KeygenMessage {
    #[n(0)]
    Part1(#[n(0)] Part1Message),
    #[n(1)]
    Part2(#[n(0)] Part2Message),
    #[n(2)]
    Done(#[n(0)] DoneMessage),
}

#[derive(Clone, Debug)]
pub struct GeneratedKeys(pub KeyPackage, pub PublicKeyPackage);

struct BroadcastState {
    sender: NetworkSender<KeygenMessage>,
    session_id: String,
    round1_package: round1::Package,
    round2_packages: BTreeMap<NodeId, round2::Package>,
    done: bool,
}

impl BroadcastState {
    fn new(sender: NetworkSender<KeygenMessage>, round1_package: round1::Package) -> Self {
        Self {
            sender,
            session_id: "".into(),
            round1_package,
            round2_packages: BTreeMap::new(),
            done: false,
        }
    }

    async fn broadcast_round_2(
        &mut self,
        session_id: &str,
        round2_packages: BTreeMap<NodeId, round2::Package>,
    ) {
        self.session_id = session_id.to_string();
        self.round2_packages = round2_packages;
        self.done = false;
        self.send().await;
    }

    async fn broadcast_done(&mut self, session_id: &str) {
        assert!(self.session_id == session_id);
        self.done = true;
        self.send().await;
    }

    async fn send(&self) {
        let sender = self.sender.clone();
        let round1_package = self.round1_package.clone();
        let broadcast_round1 = async move {
            // We should always be broadcasting the package from round 1,
            // in case a node disconnected and reconnected
            sender
                .broadcast(KeygenMessage::Part1(Part1Message {
                    package: Box::new(round1_package.into()),
                }))
                .await;
        };

        let sender = self.sender.clone();
        let session_id = self.session_id.clone();
        let round2_packages = self.round2_packages.clone();
        let broadcast_round2 = async move {
            // We should sending out the pacakges from round 2 once we have them
            let tasks = round2_packages.into_iter().map(|(node_id, package)| {
                sender.send(
                    node_id,
                    KeygenMessage::Part2(Part2Message {
                        session_id: session_id.clone(),
                        package: Box::new(package.into()),
                    }),
                )
            });
            join_all(tasks).await;
        };

        let sender = self.sender.clone();
        let session_id = self.session_id.clone();
        let broadcast_done = async move {
            // If we have finished the DKG process, let the world know
            if self.done {
                sender
                    .broadcast(KeygenMessage::Done(DoneMessage { session_id }))
                    .await
            }
        };

        join!(broadcast_round1, broadcast_round2, broadcast_done);
    }
}

pub async fn run(
    id: NodeId,
    channel: NetworkChannel<KeygenMessage>,
    max_signers: u16,
    min_signers: u16,
) -> Result<GeneratedKeys> {
    let peers_count = max_signers as usize - 1;

    let (sender, mut receiver) = channel.split();

    // use our ID to get a stable/unique cryptographic identifier
    let identifier = Identifier::derive(id.as_bytes())?;

    // DKG has three rounds. We can perform round 1 on our own, it gets us information needed for round 2
    let (round1_secret_package, round1_package) = {
        let rng = thread_rng();
        part1(identifier, max_signers, min_signers, rng)?
    };

    let broadcast_state = Arc::new(RwLock::new(BroadcastState::new(
        sender.clone(),
        round1_package.clone(),
    )));

    let bc_state = broadcast_state.clone();
    let broadcast_handle = async move {
        loop {
            let state = bc_state.read().await;
            state.send().await;
            drop(state);
            sleep(Duration::from_secs(1)).await
        }
    };

    let receive_task = async move {
        // A "session id" is a hash of all the packages generated during round 1 of the DKG process.
        // Every node needs to agree on the "session id" during round 2 and beyond, so that state from
        // any older DKG session doesn't bleed over.
        // If a node quits and rejoins during DKG generation, it'll send a new round 1 package to every participant,
        // so they'll all generate a new "session id".
        let mut session_id = String::new();
        let mut round1_hashes = BTreeMap::new();
        round1_hashes.insert(
            identifier,
            round1_package
                .serialize()
                .expect("error serializing package"),
        );

        let mut round1_packages = BTreeMap::new();
        let mut round2_secret_package: Option<round2::SecretPackage> = None;
        let mut round2_packages = BTreeMap::new();

        // Track which nodes have said they've finished in each session, so that we know when to disconnect the network.
        let mut done_sets = HashMap::<String, HashSet<NodeId>>::new();
        let mut generated_keys = None;

        let mut identifier_lookup: BTreeMap<Identifier, NodeId> = BTreeMap::new();

        while let Some(message) = receiver.recv().await {
            let from = message.from;

            // The DKG algorithm uses Identifier to identify a node, but our network uses NodeId
            // Maintain a lookup for later.
            let from_id = Identifier::derive(from.as_bytes())?;
            identifier_lookup.insert(from_id, from.clone());

            match message.data {
                KeygenMessage::Part1(Part1Message { package }) => {
                    let package: round1::Package = (*package).into();
                    if round1_packages.get(&from_id) == Some(&package) {
                        // We've seen this one
                        continue;
                    }
                    debug!(%from, "Received new round 1 package");

                    round1_hashes.insert(
                        from_id,
                        package.serialize().expect("error serializing package"),
                    );
                    round1_packages.insert(from_id, package);
                    if round1_packages.len() == peers_count {
                        // We have packages from every peer, and now we can start (or re-start) round 2
                        session_id = compute_session_id(&round1_hashes);
                        info!(session_id, "Round 1 complete! Beginning round 2, waiting for all peers to send round 2 packages");

                        round2_packages.clear();

                        let (secret_package, outgoing_packages) =
                            part2(round1_secret_package.clone(), &round1_packages)?;
                        round2_secret_package.replace(secret_package);

                        let round2_packages = outgoing_packages
                            .into_iter()
                            .map(|(identifier, package)| {
                                let id = identifier_lookup.get(&identifier).unwrap().clone();
                                (id, package)
                            })
                            .collect();

                        broadcast_state
                            .write()
                            .await
                            .broadcast_round_2(&session_id, round2_packages)
                            .await;
                    }
                }
                KeygenMessage::Part2(Part2Message {
                    session_id: other_session_id,
                    package,
                }) => {
                    if session_id != other_session_id {
                        trace!(
                            session_id,
                            other_session_id,
                            "Round 2 message received from another session"
                        );
                        continue;
                    }

                    let package: round2::Package = (*package).into();
                    if round2_packages.get(&from_id) == Some(&package) {
                        // We've seen this one
                        continue;
                    }
                    debug!(%from, session_id, "Received new round 2 package");

                    round2_packages.insert(from_id, package);
                    if round2_packages.len() == peers_count {
                        // We have everything we need to compute our frost keys
                        let round2_secret_package = round2_secret_package.as_ref().unwrap();
                        let (key_package, public_key_package) =
                            part3(round2_secret_package, &round1_packages, &round2_packages)?;
                        info!(session_id, "Key generation complete! Waiting for all peers to confirm they have generated keys as well");

                        generated_keys.replace(GeneratedKeys(key_package, public_key_package));
                        done_sets
                            .entry(session_id.clone())
                            .or_default()
                            .insert(id.clone());
                        broadcast_state
                            .write()
                            .await
                            .broadcast_done(&session_id)
                            .await;
                    }
                }
                KeygenMessage::Done(DoneMessage { session_id }) => {
                    debug!(%from, session_id, "Received new done message");
                    done_sets.entry(session_id).or_default().insert(from);
                }
            }

            // If every node has finished generating keys, shut this down.
            if done_sets
                .get(&session_id)
                .is_some_and(|set| set.len() == max_signers as usize)
            {
                // We're done!
                info!("All peers have generated keys!");
                sleep(Duration::from_secs(1)).await;

                return generated_keys.ok_or(anyhow!("Generated keys missing"));
            }
        }

        Err(anyhow!("Network closed before we could generate keys"))
    };

    select! {
        _ = broadcast_handle => { Err(anyhow!("Broadcasting task ended early")) }
        result = receive_task => { result }
    }
}

fn compute_session_id(round1_hashes: &BTreeMap<Identifier, Vec<u8>>) -> String {
    let mut hasher = Hasher::<160>::new();
    for (identifier, hash) in round1_hashes {
        hasher.input(&identifier.serialize());
        hasher.input(hash);
    }
    hasher.finalize().to_string()
}
