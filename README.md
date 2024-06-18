# Oracles Offchain

## Setup

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

## Running

```sh
# If you have your own cardano node, you can point to its IPC directory
IPC_DIR=/path/to/ipc docker compose up -d

# If you don't have a cardano node (note that spinning one up takes hours)
docker compose --profile standalone up -d

```