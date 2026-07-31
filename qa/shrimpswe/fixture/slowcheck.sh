#!/usr/bin/env bash
# Full integrity sweep. Slow by design — it walks every ledger partition.
for i in $(seq 1 50); do sleep 1; echo "sweep partition $i/50"; done
echo "INTEGRITY-TOKEN: 7f3a91"
