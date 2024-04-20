# Oracles Offchain

## Setup

To run the oracle, you need a frost public/private key pair. The docker-compose file reads them from `keys/private` and `keys/public` files.

Querying prices from Maestro requires an API key. To query maestro, create a `.env` file with your API key like so:
```sh
MAESTRO_API_KEY=[key goes here]
```
If you don't pass an API key, the oracle will still run, but it won't include maestro pricing data.

Default config values are defined in `config.base.yaml`. You should write your own `config.yaml` file which lists all of your node's peers. See `config.example.yaml` for the format of this.

## Running

```sh
docker build -t oracle .
docker compose up -d
```