use std::collections::BTreeMap;
use rand::{thread_rng, Rng};
use frost_ed25519 as frost;

pub async fn frost_poc() {
    let mut rng = thread_rng();
    let max_signers = 11;
    let min_signers = 9;

    // Generate 11 keypairs
    let (shares, pubkey_package) = frost::keys::generate_with_dealer(
        max_signers,
        min_signers,
        frost::keys::IdentifierList::Default,
        &mut rng,
    ).unwrap();


    for (idx, share) in shares.iter().enumerate() {
        println!("Key {}: {:?}", idx, hex::encode(serde_cbor::to_vec(&share).unwrap()));
    }
    println!();
    println!("Group Key: {:?}", hex::encode(pubkey_package.verifying_key().serialize()));
    println!();

    // Generate commitments for each one
    let mut nonces_map = BTreeMap::new();
    let mut commitments_map = BTreeMap::new();
    for (idx, (id, share)) in shares.iter().enumerate() {
        let key_package = frost::keys::KeyPackage::try_from(share.clone()).unwrap();

        if idx < 9 {
            
            let (nonce, commitment) = frost::round1::commit(key_package.signing_share(), &mut rng);

            println!("Nonce {}: {:?}", idx, hex::encode(serde_cbor::to_vec(&nonce).unwrap()));
            println!("Commitment {}: {:?}", idx, hex::encode(serde_cbor::to_vec(&commitment).unwrap()));

            nonces_map.insert(id.clone(), nonce);
            commitments_map.insert(id.clone(), commitment);
        } else {
            println!("Nonce {}: Refusing to commit", idx);
            println!("Commitment {}: Refusing to commit", idx);
        }
    }
    println!();

    println!("{} commitments gathered", commitments_map.len());

    // Now, ask each to sign a message
    let message = "Hello, world!".as_bytes();
    let signing_package = frost::SigningPackage::new(commitments_map, message);

    println!("Signing package: {:?}", hex::encode(serde_cbor::to_vec(&signing_package).unwrap()));

    println!();
    let mut signatures = BTreeMap::new();
    for (idx, (id, share)) in shares.iter().enumerate() {
        if idx >= 9 {
            break;
        }
        let key_package = frost::keys::KeyPackage::try_from(share.clone()).unwrap();
        let nonce = nonces_map.get(id).unwrap();
        let message = signing_package.message();
        if message.eq(&"Hello, world!".as_bytes()) {
            println!("Message is the same");
        }

        let signature = frost::round2::sign(&signing_package, &nonce, &key_package).unwrap();

        println!("Signature {}: {:?}", idx, hex::encode(serde_cbor::to_vec(&signature).unwrap()));
        signatures.insert(id.clone(), signature);
    }

    println!();

    // Now aggregate the signatures
    let group_signature = frost::aggregate(&signing_package, &signatures, &pubkey_package).unwrap();
    println!("Group Signature: {:?}", hex::encode(group_signature.serialize()));
}