use std::collections::BTreeMap;

use anyhow::Result;
use clap::Parser;
use criterion::{Criterion, criterion_group, criterion_main};
use frost_ed25519::{
    Identifier, SigningPackage, aggregate,
    keys::{self, IdentifierList, KeyPackage, PublicKeyPackage, SecretShare},
    round1, round2,
};
use rand::{rngs::ThreadRng, thread_rng};

#[derive(Parser)]
struct Args {
    #[clap(long)]
    signer_count: usize,
    #[clap(long)]
    signed_packages: usize,
}

struct Node {
    identifier: Identifier,
    secret_key: SecretShare,
    key_package: KeyPackage,
}

fn generate_nodes(count: usize) -> Result<(Vec<Node>, PublicKeyPackage)> {
    let rng = thread_rng();
    let (shares, pubkey_package) =
        keys::generate_with_dealer(count as u16, count as u16, IdentifierList::Default, rng)?;
    let nodes = shares
        .into_iter()
        .map(|(identifier, secret_key)| {
            let (verifying_share, verifying_key) = secret_key.verify().unwrap();
            let key_package = KeyPackage::new(
                identifier,
                *secret_key.signing_share(),
                verifying_share,
                verifying_key,
                count as u16,
            );
            Node {
                identifier,
                secret_key,
                key_package,
            }
        })
        .collect();
    Ok((nodes, pubkey_package))
}

fn sign_package(nodes: &[Node], pubkeys: &PublicKeyPackage, rng: &mut ThreadRng) -> Result<()> {
    let mut nonces = vec![];
    let mut commitments = BTreeMap::new();
    for node in nodes {
        let (nonce, commitment) = round1::commit(node.secret_key.signing_share(), rng);
        nonces.push(nonce);
        commitments.insert(node.identifier, commitment);
    }
    let signing_package = SigningPackage::new(commitments, b"Hello world!");

    let mut signature_shares = BTreeMap::new();
    for (node, nonce) in nodes.iter().zip(nonces) {
        let signature = round2::sign(&signing_package, &nonce, &node.key_package)?;
        signature_shares.insert(node.identifier, signature);
    }

    aggregate(&signing_package, &signature_shares, pubkeys)?;
    Ok(())
}

fn benchmark_size(c: &mut Criterion, count: usize) {
    let id = format!("{count} nodes");
    c.bench_function(&id, |b| {
        let (nodes, pubkeys) = generate_nodes(count).unwrap();
        let mut rng = thread_rng();
        b.iter(|| sign_package(&nodes, &pubkeys, &mut rng).unwrap());
    });
}

fn benchmark(c: &mut Criterion) {
    benchmark_size(c, 3);
    benchmark_size(c, 10);
    benchmark_size(c, 100);
    benchmark_size(c, 200);
}

criterion_group!(benches, benchmark);
criterion_main!(benches);
