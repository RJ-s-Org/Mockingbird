#!/usr/bin/env bash
# Stands in for an app under test: it calls the real API host over HTTPS
# and relies on the environment the action exported (HTTPS_PROXY and the
# CA bundle), with no Mockingbird-specific code.
set -euo pipefail
curl -fsS --cacert "$SSL_CERT_FILE" https://api.orders.test/orders/o1 | jq -e '.id'
curl -fsS --cacert "$SSL_CERT_FILE" https://api.orders.test/orders | jq -e 'type == "array"'
curl -fsS "$MOCKINGBIRD_URL/orders/o2" -H 'Host: api.orders.test' | jq -e '.id'
