use std::{
    collections::{BTreeMap, HashSet},
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
use rand::thread_rng;
use serde::{Deserialize, Serialize};
use tokio::{select, sync::watch, task::spawn_blocking, time::sleep};
use tracing::{info, Instrument};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Part1Message {
    package: Box<round1::Package>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Part2Message {
    package: Box<round2::Package>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum KeygenMessage {
    Part1(Part1Message),
    Part2(Part2Message),
    Done,
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

    let (mut done_tx, mut done_rx) = mpsc::channel(1);

    // Run our network layer in the background after setting up a "keygen" channel
    let channel = network.keygen_channel();
    tokio::spawn(
        async move {
            select! {
                result = network.listen() => { result.unwrap(); }
                _ = done_rx.next() => {  }
            };
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

    // Keep broadcasting our round 1 payload until everyone has finished (in case someone disconnects or joins late)
    let part1_sender = sender.clone();
    let part1_broadcast_handle = tokio::spawn(
        async move {
            loop {
                let package = Box::new(round1_package.clone());
                let message = Part1Message { package };
                part1_sender.broadcast(KeygenMessage::Part1(message)).await;
                sleep(Duration::from_secs(1)).await;
            }
        }
        .in_current_span(),
    );

    // Also send our round 2 payloads (once we have them)
    let (outgoing_round2_packages_tx, outgoing_round2_packages_rx) =
        watch::channel(BTreeMap::new());
    let identifiers = identifier_lookup.clone();
    let part2_sender = sender.clone();
    let part2_broadcast_handle = tokio::spawn(
        async move {
            loop {
                // clone this so we aren't holding a lock for too long
                let round2_packages = outgoing_round2_packages_rx.borrow().clone();
                for (identifier, package) in round2_packages {
                    let to: NodeId = identifiers.get(&identifier).unwrap().clone();
                    let message = Part2Message {
                        package: Box::new(package),
                    };
                    part2_sender.send(to, KeygenMessage::Part2(message)).await;
                }
                sleep(Duration::from_secs(1)).await;
            }
        }
        .in_current_span(),
    );

    let mut round1_packages = BTreeMap::new();
    let mut round2_secret_package: Option<round2::SecretPackage> = None;
    let mut round2_packages = BTreeMap::new();

    // track which nodes have said they've finished, so that we know when to disconnect the network.
    let mut done_set = HashSet::new();

    // And now that we've got our senders all set up, we're ready to run our receiver logic
    while let Some(message) = receiver.recv().await {
        let from = message.from;

        // The DKG algorithm uses Identifier to identify a node, but our network uses NodeId
        // Maintain a lookup for later.
        let from_id = Identifier::derive(from.as_bytes())?;
        identifier_lookup.insert(from_id, from.clone());

        match message.data {
            KeygenMessage::Part1(Part1Message { package }) => {
                if round1_packages.get(&from_id) == Some(&*package) {
                    // We've seen this one
                    continue;
                }

                round1_packages.insert(from_id, *package);
                if round1_packages.len() == peers_count {
                    info!("Round 1 complete! Beginning round 2");
                    // We have packages from every peer, and now we can start (or re-start) round 2
                    let (secret_package, outgoing_packages) =
                        part2(round1_secret_package.clone(), &round1_packages)?;
                    round2_secret_package.replace(secret_package);
                    outgoing_round2_packages_tx.send_replace(outgoing_packages);
                }
            }
            KeygenMessage::Part2(Part2Message { package }) => {
                if round2_packages.get(&from_id) == Some(&*package) {
                    // We've seen this one
                    continue;
                }

                round2_packages.insert(from_id, *package);
                if round2_packages.len() == peers_count {
                    // We have everything we need to compute our frost keys
                    let round2_secret_package = round2_secret_package.as_ref().unwrap();
                    let (key_package, public_key_package) =
                        part3(round2_secret_package, &round1_packages, &round2_packages)?;
                    info!("Key generation complete!");
                    let address =
                        keys::write_frost_keys(&keys_dir, key_package, public_key_package)
                            .context("Could not save frost keys")?;
                    info!("The new frost address is: {}", address);
                    sender.broadcast(KeygenMessage::Done).await;
                }
            }
            KeygenMessage::Done => {
                done_set.insert(from);
                if done_set.len() == peers_count {
                    info!("All peers have generated their keys!");
                    sleep(Duration::from_secs(3)).await;
                    info!("Press enter to shut down.");
                    part1_broadcast_handle.abort();
                    part2_broadcast_handle.abort();
                    done_tx.send(()).await.unwrap();
                    spawn_blocking(|| {
                        std::io::stdin().read_line(&mut String::new()).unwrap();
                    })
                    .await
                    .unwrap();
                }
            }
        }
    }

    Ok(())
}
