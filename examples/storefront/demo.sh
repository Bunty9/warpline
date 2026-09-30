#!/usr/bin/env bash
# End-to-end walkthrough against a running stack, asserting every step.
#   ./demo.sh          stack is already up (docker compose up -d --build)
#   ./demo.sh --up     bring it up first
# Overrides: STOREFRONT (default http://localhost:3000), ADMIN_TOKEN (default
# demo-admin-token, as in docker-compose.yml), FRAUD_HOST (default fraud-mock,
# the host name the storefront reaches the fraud service by; use 127.0.0.1
# when running the binaries locally).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

STOREFRONT="${STOREFRONT:-http://localhost:3000}"
ADMIN_TOKEN="${ADMIN_TOKEN:-demo-admin-token}"
FRAUD_HOST="${FRAUD_HOST:-fraud-mock}"
HOOK=app/tests/fixtures/checkout_hook.wasm
RUN="$(date +%s)$RANDOM"
SHOP="shop-$RUN"
BARE="bare-$RUN"

if [[ "${1:-}" == "--up" ]]; then
    docker compose up -d --build
fi

fail() { echo "FAIL: $*" >&2; exit 1; }
step() { echo "==> $*"; }

# req METHOD PATH [curl args...]: sets $STATUS and $BODY.
req() {
    local method=$1 path=$2; shift 2
    local out
    out=$(curl -sS -X "$method" -w $'\n%{http_code}' "$@" "$STOREFRONT$path")
    STATUS=${out##*$'\n'}
    BODY=${out%$'\n'*}
}
# field JSON-BODY EXPR: print a field via python (no jq needed).
field() { python3 -c 'import json,sys; d=json.loads(sys.argv[1]); print(eval(sys.argv[2]))' "$1" "$2"; }
expect_status() { [[ $STATUS == "$1" ]] || fail "$2: expected HTTP $1, got $STATUS: $BODY"; }
expect_field() { [[ "$(field "$BODY" "$1")" == "$2" ]] || fail "$3: expected $1 == $2 in $BODY"; }

checkout() { # merchant customer
    req POST "/shops/$1/checkout" -H 'content-type: application/json' \
        -d "{\"customer\":\"$2\",\"items\":[{\"sku\":\"mug\",\"qty\":2,\"unit_cents\":1000}]}"
    expect_status 200 "checkout $2"
}

step "waiting for the storefront at $STOREFRONT"
for _ in $(seq 60); do
    curl -fsS "$STOREFRONT/healthz" >/dev/null 2>&1 && break
    sleep 1
done
curl -fsS "$STOREFRONT/healthz" >/dev/null || fail "storefront not reachable"
[[ -f $HOOK ]] || fail "$HOOK missing (run ./build.sh)"

step "1. create merchant $SHOP (allowed host: $FRAUD_HOST)"
req POST /admin/merchants -H "authorization: Bearer $ADMIN_TOKEN" -H 'content-type: application/json' \
    -d "{\"name\":\"$SHOP\",\"allowed_hosts\":[\"$FRAUD_HOST\"]}"
expect_status 201 "create merchant"
KEY=$(field "$BODY" 'd["api_key"]')
echo "    got API key ${KEY:0:7}..."

step "2. upload the checkout hook"
req PUT "/merchant/$SHOP/hook" -H "authorization: Bearer $KEY" --data-binary "@$HOOK"
expect_status 200 "upload hook"
echo "    digest $(field "$BODY" 'd["digest"][:12]')..."

step "3. alice checks out three times (2000 cents each): loyalty discount from order 3"
for expect in 0 0 200; do
    checkout "$SHOP" alice
    expect_field 'd["approved"]' True "alice approved"
    expect_field 'd["discount_cents"]' "$expect" "alice discount"
    echo "    order -> discount $(field "$BODY" 'd["discount_cents"]'), total $(field "$BODY" 'd["total_cents"]'): $(field "$BODY" 'd["message"]')"
done

step "4. risky-bob is rejected by the fraud check; carol is a normal customer"
checkout "$SHOP" risky-bob
expect_field 'd["approved"]' False "risky-bob rejected"
echo "    risky-bob -> $(field "$BODY" 'd["message"]')"
checkout "$SHOP" carol
expect_field 'd["approved"]' True "carol approved"

step "5. merchant $BARE has no allowed hosts: fraud fetch fails, order still approved"
req POST /admin/merchants -H "authorization: Bearer $ADMIN_TOKEN" -H 'content-type: application/json' \
    -d "{\"name\":\"$BARE\"}"
expect_status 201 "create second merchant"
KEY2=$(field "$BODY" 'd["api_key"]')
req PUT "/merchant/$BARE/hook" -H "authorization: Bearer $KEY2" --data-binary "@$HOOK"
expect_status 200 "upload hook (second merchant)"
checkout "$BARE" risky-bob
expect_field 'd["approved"]' True "unchecked order approved"
expect_field 'd["message"]' "fraud check unavailable" "fallback message"
echo "    risky-bob -> $(field "$BODY" 'd["message"]')"

step "6. uploading invalid wasm is rejected"
req PUT "/merchant/$SHOP/hook" -H "authorization: Bearer $KEY" --data-binary "this is not wasm"
expect_status 422 "invalid wasm"
echo "    HTTP $STATUS"

step "7. usage summary for $SHOP (metering is batched, so poll briefly)"
N=0
for _ in $(seq 30); do
    req GET "/merchant/$SHOP/usage" -H "authorization: Bearer $KEY"
    expect_status 200 "usage"
    N=$(field "$BODY" 'd["invocations"]')
    (( N >= 5 )) && break
    sleep 0.5
done
(( N >= 5 )) || fail "expected at least 5 invocations, saw $N"
echo "    $BODY"

echo "OK: all steps passed"
