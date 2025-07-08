#!/bin/bash

# Script to run Kupo locally using Docker
# This connects to the Cardano node socket and exposes Kupo on port 1442

docker run -d \
  --name kupo-local \
  -p 1442:1442 \
  -v /Users/rjlacanlaled/.dmtr/tmp/faithful-category-622fa8/mainnet-zuist1.socket:/node.socket \
  -v $(pwd)/mainnet-config:/config \
  -v $(pwd)/volumes/kupo-db:/db \
  cardanosolutions/kupo \
  --node-socket /node.socket \
  --node-config /config/config.json \
  --since 148027022.9b06accfd37ecbeecd5a1c7bc12c70381cd932e5ae07883f19368d634d584a53 \
  --host 0.0.0.0 \
  --match "e0302560ced2fdcbfcb2602697df970cd0d6a38f94b32703f51c312b/*" \
  --match "e1317b152faac13426e6a83e06ff88a4d62cce3c1634ab0a5ec13309/*" \
  --match "6b9c456aa650cb808a9ab54326e039d5235ed69f069c9664a8fe5b69/*" \
  --workdir /db \
  --defer-db-indexes \
  --prune-utxo

echo "Kupo started. Check logs with: docker logs -f kupo-local"
echo "Kupo API available at: http://localhost:1442"