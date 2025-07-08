# Butane Oracle

This repository holds the off-chain code that provides an oracle feed for the Butane protocol.

## Introduction

The Butane protocol allows users to lock up assets as collateral, and mint some amount of synthetic asset. For example, locking up $100 worth of ADA, might let you mint $50 USDB. Since the on-chain code cannot interact with the outside world to know the price of ADA in USD is, it relies on a trusted "oracle feed" to tell it those prices.

In order to be robust to outages, bugs, or malicious actors manipulating those prices, this oracle needs to employ a variety of techniques to keep the Butane protocol secure and highly solvent.

## Architecture

This section will explain, hopefully in relatively non-technical terms, how the oracle network operates, and what mechanisms it has in place for ensuring a reliable oracle feed for the purposes of preventing bad debt in the Butane protocol.

It also describes the protocol as it exists today, with only light nods to the system that Butane intends to eventually build.

The Butane oracle network consists of a set of operators. These operators have their own public/private keypair, known to the other nodes, that allow them to communicate in a secure and authenticated way.

Additionally, these node operators have performed a multi-party computation to establish a shared [FROST](https://github.com/ZcashFoundation/frost) keypair. This key-pair is in the form of a public key, known to everyone, and a set of private key shares. Each node operator has one key share. More on what these are used for later.

These operators establish connections directly to each other node. At any given moment, one of these nodes is acting as the "leader". If the leader goes offline, or is otherwise unreachable for too long, the rest of the nodes use a consensus protocol called [Raft](https://raft.github.io/) to decide on a new leader. This failover typically happens within a few hundred milliseconds.

Meanwhile, each node is continually querying a number of different data sources, such as binance, coinbase, and on-chain DEX's for up to date pricing information. This includes pricing for the collateral that is accepted by the Butane protocol, the synthetic assets minted by the Butane protocol, and the underlying assets that the Butane protocol tries to track. Thus, at any given moment, the node has its _own_ view of these markets.

Pricing information is collected in terms of trading pairs (i.e. SundaeSwap reports a price for BTN in ADA from a BTN-ADA pool), and we use those to compute a price in USD for each token. For any given token, the oracle uses an average of all collected prices (weighted by TVL) to decide the exact price to report.

Every 10 seconds, assuming the network is healthy, the leader will "propose" a value for each synthetic, listing out the collateral prices. Each node will compare that proposed value to their own. If it is within some tight threshold, they will agree to sign the payload, otherwise they will refuse.

Using the FROST keys from above, if a certain threshold of the nodes agree to sign the payload, they can collaborate to produce a signature. This signature can be validated as if it were a signature corresponding to the FROST public key.

These nodes will then serve this signed payload via an API; anyone wishing to interact with the Butane protocol can ask for the latest signed payload from any of the nodes. The butane smart contracts have the appropriate public key, and thus can check that the oracle feed came from the trusted node operators.

Over time, the size of this set can be expanded and evolved, allowing for a more robust and decentralized oracle network.

So, how does a node decide if it will sign the payload or not? There are a number of safety measures in place to ensure the Butane protocol does not take on bad debt:

- The node confirms that its own pricing data is up to date by comparing multiple different sources of timing information.
- The node confirms that the messages from each other node are authenticated with the appropriate key pair.
- The node confirms that their own pricing data is high confidence from a minimum number of sources, using a weighted average price from each source.
- The node adjusts the reported price differently depending on the change: drops in prices are seen by the oracle feed immediately, to liquidate quickly and guard against bad debt; while rises in prices are phased in over time, to ensure that short lived spikes don't get used to under-collateralize a synthetic.

## Butane Deployment

For the Butane mainnet deployment, currently we aggregate data from the following sources:

- Binance
- Bybit
- Coinbase
- Crypto.com
- FXRatesAPI
- Kucoin
- Minswap v1+v2
- OKX
- Splash
- Sundae v1+v3
- VyFi
- WingRiders v1+v2

The oracle network is currently a set of 5 nodes, requiring agreement among 3 of them to produce a signature. The current operators of those nodes:

- TxPipe
- Blink Labs
- Sundae Labs
- Easy1
- Butane

## Setup

This section will explain how to set up and run an oracle node, for the node operators.

### Pick a data directory

The oracle saves some state to disk, to help it start up more gracefully after restarting. It should have access to a writable directory where it can keep that state. By default, this is a `data` directory relative to your PWD, but you can set a `DATA_DIRECTORY` env var to change that.

### Pick a key directory

The oracle uses several sets of private/public keys. By default, these are stored in a `keys` directory relative to your PWD, but you can set a `KEYS_DIRECTORY` env var to change that.

### Generate a private key

Generate an ED25519 private key, stored in PEM format. The oracle will look for this in `${KEYS_DIRECTORY}/private.pem`.

```sh
openssl genpkey -algorithm ed25519 -out ./keys/private.pem
```

The other node owners will need your public key to finish their setup. You can get that in PEM form with openssl as well:

```sh
openssl pkey -in ./keys/private.pem -pubout
```

### Configure your peers

Create a `config.yaml` file. List the address and public key of every peer your node connects to. See `config.example.yaml` for an example of the format.

If you want, you can also override any default config settings (which are defined in `config.base.yaml`).

### Generate FROST keys

The oracle needs a FROST key pair in order to sign payloads. These keys are stored in `KEYS_DIRECTORY`. You have two ways to generate these keys:

#### With DKG

This oracle supports a "keygen mode". If every node is running in "keygen mode", they will collectively generate a new set of keys and store them to disk.

To initiate DKG, set `keygen.enabled` to `true` in your `config.yaml`. Make sure that every node agrees on the value of `keygen.min_signers`, or DKG will fail.

Once every node is online, they will finish DKG and store the keys to disk. Once those files exist, you should update your configuration with a new `frost_address` and disable keygen mode (by setting `keygen.enabled` to `false`).

#### Without DKG

You can generate a set of FROST keys with the keygen command:

```sh
cargo run --bin keygen -- --min-signers 2 --max-signers 3
```

These will be saved in subdirectories of `KEYS_DIRECTORY`.

### Set up Maestro

Querying prices from Maestro requires an API key. To query Maestro, create a `.env` file with your API key like so:

```sh
MAESTRO_API_KEY=[key goes here]
```

If you don't pass an API key, the oracle will still run, but it won't include maestro pricing data.

### Set up FXRatesAPI

FXRatesAPI needs an API key as well. To get a key for this API, visit https://fxratesapi.com/auth/signup and create an account. The free tier is fine.

The oracle reads the key from the environment variable `FXRATESAPI_API_KEY`. You can add it to your `.env` file:

```sh
FXRATESAPI_API_KEY=[key goes here]
```

The oracle will run without an FXRatesAPI key, but this API is currently the only source of truth for some fiat currencies, so it is highly recommended to create one.

## Running

```sh
# If you have your own cardano node, you can point to its IPC directory
IPC_DIR=/path/to/ipc docker compose up -d

# If you don't have a cardano node (note that spinning one up takes hours)
docker compose --profile standalone up -d

```

`docker compose -f docker-compose.multi-node.yaml up -d`
