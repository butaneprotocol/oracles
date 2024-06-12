use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use crate::{
    config::OracleConfig,
    keys::{self, get_keys_directory},
    network::{Network, NodeId},
};
use anyhow::{anyhow, Context, Result};
use dashmap::DashMap;
use frost_ed25519::{
    keys::dkg::{part1, part2, part3, round1, round2},
    Identifier,
};
use futures::{channel::mpsc, SinkExt, StreamExt};
use minicbor::{Decode, Encode};
use pallas_crypto::hash::Hasher;
use rand::thread_rng;
use tokio::{
    select,
    sync::{oneshot, watch, RwLock},
    task::spawn_blocking,
    time::sleep,
};
use tracing::{info, trace, Instrument};

#[derive(Decode, Encode, Clone, Debug)]
pub struct Part1Message {
    #[n(0)]
    package: Vec<u8>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct Part2Message {
    #[n(0)]
    session_id: String,
    #[n(1)]
    package: Vec<u8>,
}

#[derive(Decode, Encode, Clone, Debug)]
pub struct DoneMessage {
    #[n(0)]
    session_id: String,
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

struct CompletionReport {
    session_id: String,
    node_id: NodeId,
}

pub async fn run(config: &OracleConfig) -> Result<()> {
    let mut network = Network::new(config)?;
    let peers_count = network.peers_count();
    let id = network.id.clone();

    let keys_dir = get_keys_directory()?;

    // Log where the keys will be saved
    info!(
        "Running in DKG mode. Will generate frost keys into {}.",
        keys_dir.display()
    );

    let identifier_lookup: Arc<DashMap<Identifier, NodeId>> = Arc::new(DashMap::new());

    let (shutdown_network_tx, shutdown_network_rx) = oneshot::channel();

    // Run our network layer in the background after setting up a "keygen" channel
    let channel = network.keygen_channel();
    tokio::spawn(
        async move {
            select! {
                result = network.listen() => { result.unwrap(); }
                _ = shutdown_network_rx => {  }
            };
            trace!("Network shut down");
        }
        .in_current_span(),
    );
    let (sender, mut receiver) = channel.split();

    // use our ID to get a stable/unique cryptographic identifier
    let identifier = Identifier::derive(id.as_bytes())?;

    // DKG has three rounds. We can perform round 1 on our own, it gets us information needed for round 2
    let (round1_secret_package, round1_package) = {
        let rng = thread_rng();
        let max_signers = 1 + peers_count as u16;
        let min_signers = config
            .keygen
            .min_signers
            .ok_or_else(|| anyhow!("Must specify min_signers"))?;
        part1(identifier, max_signers, min_signers, rng)?
    };

    // A "session id" is a hash of all the packages generated during round 1 of the DKG process.
    // Every node needs to agree on the "session id" during round 2 and beyond, so that state from
    // any older DKG session doesn't bleed over.
    // If a node quits and rejoins during DKG generation, it'll send a new round 1 package to every participant,
    // so they'll all generate a new "session id".
    let current_session_id = Arc::new(RwLock::new(String::new()));
    let mut round1_hashes = BTreeMap::new();
    round1_hashes.insert(
        identifier,
        round1_package
            .serialize()
            .expect("error serializing package"),
    );

    // Keep broadcasting our round 1 payload until everyone has finished (in case someone disconnects or joins late)
    let part1_sender = sender.clone();
    let part1_broadcast_handle = tokio::spawn(
        async move {
            loop {
                let package = round1_package
                    .serialize()
                    .expect("error serializing round 1 package");
                let message = Part1Message { package };
                part1_sender.broadcast(KeygenMessage::Part1(message)).await;
                sleep(Duration::from_secs(1)).await;
            }
        }
        .in_current_span(),
    );

    // Send our round 2 payloads (once we have them)
    let (outgoing_round2_packages_tx, outgoing_round2_packages_rx) =
        watch::channel(BTreeMap::<Identifier, round2::Package>::new());
    let identifiers = identifier_lookup.clone();
    let part2_session_id = current_session_id.clone();
    let part2_sender = sender.clone();
    let part2_broadcast_handle = tokio::spawn(
        async move {
            loop {
                // clone this so we aren't holding a lock for too long
                let round2_packages = outgoing_round2_packages_rx.borrow().clone();
                for (identifier, package) in round2_packages {
                    let to: NodeId = identifiers.get(&identifier).unwrap().clone();
                    let message = Part2Message {
                        session_id: part2_session_id.read().await.clone(),
                        package: package
                            .serialize()
                            .expect("error serializing round 2 package"),
                    };
                    part2_sender.send(to, KeygenMessage::Part2(message)).await;
                }
                sleep(Duration::from_secs(1)).await;
            }
        }
        .in_current_span(),
    );

    // We want to stop all network traffic after every node has finished generating keys.
    // Set up a task to monitor for that.
    let (mut completion_tx, mut completion_rx) = mpsc::channel(10);
    tokio::spawn(
        async move {
            // Track which nodes have said they've finished in each session, so that we know when to disconnect the network.
            let mut done_sets = HashMap::<String, HashSet<NodeId>>::new();
            while let Some(CompletionReport {
                session_id,
                node_id,
            }) = completion_rx.next().await
            {
                let done_set = done_sets.entry(session_id.clone()).or_default();
                done_set.insert(node_id);

                if done_set.len() == peers_count + 1 {
                    info!(session_id, "All nodes have generated their keys!");
                    sleep(Duration::from_secs(3)).await;
                    info!(session_id, "Press enter to shut down.");
                    part1_broadcast_handle.abort();
                    part2_broadcast_handle.abort();
                    shutdown_network_tx.send(()).unwrap();

                    // Wait for keyboard input, so these are convenient to shut down while testing locally.
                    // On a real server, this just waits "forever".
                    spawn_blocking(|| {
                        std::io::stdin().read_line(&mut String::new()).unwrap();
                    })
                    .await
                    .unwrap();
                    return;
                }
            }
        }
        .in_current_span(),
    );

    let mut round1_packages = BTreeMap::new();
    let mut round2_secret_package: Option<round2::SecretPackage> = None;
    let mut round2_packages = BTreeMap::new();

    // And now that we've got our senders all set up, we're ready to run our receiver logic
    while let Some(message) = receiver.recv().await {
        let from = message.from;

        // The DKG algorithm uses Identifier to identify a node, but our network uses NodeId
        // Maintain a lookup for later.
        let from_id = Identifier::derive(from.as_bytes())?;
        identifier_lookup.insert(from_id, from.clone());

        match message.data {
            KeygenMessage::Part1(Part1Message { package }) => {
                let package =
                    round1::Package::deserialize(&package).expect("error deserializing package");
                if round1_packages.get(&from_id) == Some(&package) {
                    // We've seen this one
                    continue;
                }

                round1_hashes.insert(
                    from_id,
                    package.serialize().expect("error serializing package"),
                );
                round1_packages.insert(from_id, package);
                if round1_packages.len() == peers_count {
                    // We have packages from every peer, and now we can start (or re-start) round 2
                    let session_id = compute_session_id(&round1_hashes);

                    // hold onto this mutex until we've finished setting up the state for round
                    let mut curr = current_session_id.write().await;
                    curr.clone_from(&session_id);
                    info!(session_id, "Round 1 complete! Beginning round 2");

                    round2_packages.clear();

                    let (secret_package, outgoing_packages) =
                        part2(round1_secret_package.clone(), &round1_packages)?;
                    round2_secret_package.replace(secret_package);
                    outgoing_round2_packages_tx.send_replace(outgoing_packages);
                }
            }
            KeygenMessage::Part2(Part2Message {
                session_id: other_session_id,
                package,
            }) => {
                let session_id = current_session_id.read().await.clone();
                if session_id != other_session_id {
                    trace!(
                        session_id,
                        other_session_id,
                        "Round 2 message received from another session"
                    );
                    continue;
                }

                let package =
                    round2::Package::deserialize(&package).expect("error deserializing package");
                if round2_packages.get(&from_id) == Some(&package) {
                    // We've seen this one
                    continue;
                }

                round2_packages.insert(from_id, package);
                if round2_packages.len() == peers_count {
                    // We have everything we need to compute our frost keys
                    let round2_secret_package = round2_secret_package.as_ref().unwrap();
                    let (key_package, public_key_package) =
                        part3(round2_secret_package, &round1_packages, &round2_packages)?;
                    info!(session_id, "Key generation complete!");
                    let address =
                        keys::write_frost_keys(&keys_dir, key_package, public_key_package)
                            .context("Could not save frost keys")?;
                    info!(session_id, "The new frost address is: {}", address);
                    completion_tx
                        .send(CompletionReport {
                            session_id: session_id.clone(),
                            node_id: id.clone(),
                        })
                        .await
                        .unwrap();
                    sender
                        .broadcast(KeygenMessage::Done(DoneMessage { session_id }))
                        .await;
                }
            }
            KeygenMessage::Done(DoneMessage { session_id }) => {
                trace!(session_id, "received a Done");
                completion_tx
                    .send(CompletionReport {
                        session_id,
                        node_id: from,
                    })
                    .await
                    .unwrap();
            }
        }
    }

    Ok(())
}

fn compute_session_id(round1_hashes: &BTreeMap<Identifier, Vec<u8>>) -> String {
    let mut hasher = Hasher::<160>::new();
    for (identifier, hash) in round1_hashes {
        hasher.input(&identifier.serialize());
        hasher.input(hash);
    }
    hasher.finalize().to_string()
}
