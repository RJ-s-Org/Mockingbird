#!/bin/sh
# Bootstraps Rauthy for the Compose stack (docker-compose.yml):
#   - groups mb:sysadmin and mb:default:admin;
#   - the confidential client `mockingbird`, whose secret is written to
#     /bootstrap/client_secret for Mockingbird to read;
#   - the demo user demo@localhost.test (password Demo-Passw0rd-123!) in both
#     groups.
# Idempotent: a 409 (already exists) counts as success, so it can run on every
# `docker compose up`. Authenticates with Rauthy's bootstrap API key.
#
# Rauthy's admin API is documented in its Swagger UI at
# http://localhost:8081/auth/v1/docs. Where a call fails, the script prints the
# manual steps (also in deploy/compose/README.md) and exits non-zero.
set -eu

RAUTHY_URL="${RAUTHY_URL:-http://localhost:8081}"
API="$RAUTHY_URL/auth/v1"
AUTH="Authorization: API-Key bootstrap\$${BOOTSTRAP_API_KEY_SECRET:?BOOTSTRAP_API_KEY_SECRET is not set}"
OUT_DIR="${OUT_DIR:-/bootstrap}"
CLIENT_ID="mockingbird"
CONSOLE="http://localhost:9090"
DEMO_EMAIL="demo@localhost.test"
DEMO_PASSWORD="Demo-Passw0rd-123!"
GROUPS_JSON='["mb:sysadmin","mb:default:admin"]'
RESP="$(mktemp)"

manual_steps() {
    cat >&2 <<EOF

rauthy-bootstrap: $1
Finish the setup by hand in the Rauthy admin UI ($API/admin, log in as
admin@localhost.test with BOOTSTRAP_ADMIN_PASSWORD_PLAIN from
docker-compose.yml), then run the stack again:
  1. Groups: create mb:sysadmin and mb:default:admin.
  2. Clients: create the confidential client '$CLIENT_ID' with redirect URI
     $CONSOLE/auth/callback and post-logout redirect URI $CONSOLE/, allow the
     scopes openid, profile, email and groups, and copy its secret into the
     bootstrap volume:
       docker compose -f deploy/compose/docker-compose.yml run --rm --entrypoint sh \\
         rauthy-bootstrap -c 'printf %s "<secret>" > /bootstrap/client_secret'
  3. Users: create $DEMO_EMAIL, set the password $DEMO_PASSWORD and add the
     user to both groups.
See deploy/compose/README.md.
EOF
    exit 1
}

# call METHOD PATH [JSON] -> prints the HTTP status; the body goes to $RESP.
call() {
    if [ $# -ge 3 ]; then
        curl -sS -o "$RESP" -w '%{http_code}' -X "$1" -H "$AUTH" \
            -H 'Content-Type: application/json' --data "$3" "$API$2"
    else
        curl -sS -o "$RESP" -w '%{http_code}' -X "$1" -H "$AUTH" "$API$2"
    fi
}

# expect STATUS WHAT: 2xx and 409 pass; anything else prints the manual steps.
expect() {
    case "$1" in
        2??) echo "rauthy-bootstrap: $2: ok ($1)" ;;
        409) echo "rauthy-bootstrap: $2: already exists" ;;
        *)
            cat "$RESP" >&2 || true
            manual_steps "$2 failed with HTTP $1"
            ;;
    esac
}

# json_field NAME: the first string value of "NAME" in $RESP.
json_field() {
    grep -o "\"$1\" *: *\"[^\"]*\"" "$RESP" | head -n 1 | sed 's/.*: *"\(.*\)"/\1/'
}

echo "rauthy-bootstrap: waiting for $RAUTHY_URL"
i=0
until curl -fsS -o /dev/null "$API/.well-known/openid-configuration"; do
    i=$((i + 1))
    if [ "$i" -ge 90 ]; then
        echo "rauthy-bootstrap: Rauthy did not come up within 180 s" >&2
        exit 1
    fi
    sleep 2
done

# Groups.
for group in mb:sysadmin mb:default:admin; do
    # verify against Rauthy docs (Swagger UI at /auth/v1/docs): POST /groups {"group": NAME}
    status="$(call POST /groups "{\"group\":\"$group\"}")"
    expect "$status" "group $group"
done

# The confidential client. New Rauthy clients allow the scopes openid,
# profile, email and groups; the check below fails if `groups` is missing.
# verify against Rauthy docs (Swagger UI at /auth/v1/docs): POST /clients (NewClientRequest)
status="$(call POST /clients "{\"id\":\"$CLIENT_ID\",\"name\":\"Mockingbird\",\"confidential\":true,\"redirect_uris\":[\"$CONSOLE/auth/callback\"],\"post_logout_redirect_uris\":[\"$CONSOLE/\"]}")"
expect "$status" "client $CLIENT_ID"
client_created=false
case "$status" in 2??) client_created=true ;; esac

# verify against Rauthy docs (Swagger UI at /auth/v1/docs): GET /clients/{id}
status="$(call GET "/clients/$CLIENT_ID")"
expect "$status" "read client $CLIENT_ID"
if ! grep -q 'groups' "$RESP"; then
    manual_steps "client $CLIENT_ID does not allow the groups scope"
fi

# Its secret: generated when the client is new or the file is missing (a
# rerun keeps the secret Mockingbird already uses).
if [ "$client_created" = true ] || [ ! -s "$OUT_DIR/client_secret" ]; then
    # verify against Rauthy docs (Swagger UI at /auth/v1/docs): PUT /clients/{id}/secret generates a new secret
    status="$(call PUT "/clients/$CLIENT_ID/secret" '{}')"
    expect "$status" "client secret"
    secret="$(json_field secret)"
    if [ -z "$secret" ]; then
        manual_steps "no secret in the response of PUT /clients/$CLIENT_ID/secret"
    fi
    mkdir -p "$OUT_DIR"
    umask 022
    printf '%s' "$secret" > "$OUT_DIR/client_secret"
    chmod 644 "$OUT_DIR/client_secret"
    echo "rauthy-bootstrap: wrote $OUT_DIR/client_secret"
fi

# The demo user.
# verify against Rauthy docs (Swagger UI at /auth/v1/docs): POST /users (NewUserRequest)
status="$(call POST /users "{\"email\":\"$DEMO_EMAIL\",\"given_name\":\"Demo\",\"family_name\":\"User\",\"language\":\"en\",\"roles\":[],\"groups\":$GROUPS_JSON}")"
expect "$status" "user $DEMO_EMAIL"
if [ "$status" = 409 ]; then
    # verify against Rauthy docs (Swagger UI at /auth/v1/docs): GET /users/email/{email}
    status="$(call GET "/users/email/$DEMO_EMAIL")"
    expect "$status" "look up user $DEMO_EMAIL"
fi
user_id="$(json_field id)"
if [ -z "$user_id" ]; then
    manual_steps "no id for user $DEMO_EMAIL"
fi

# Its password and groups (set on every run).
# verify against Rauthy docs (Swagger UI at /auth/v1/docs): PUT /users/{id} (UpdateUserRequest with password)
status="$(call PUT "/users/$user_id" "{\"email\":\"$DEMO_EMAIL\",\"given_name\":\"Demo\",\"family_name\":\"User\",\"language\":\"en\",\"password\":\"$DEMO_PASSWORD\",\"roles\":[],\"groups\":$GROUPS_JSON,\"enabled\":true,\"email_verified\":true}")"
expect "$status" "password of $DEMO_EMAIL"

rm -f "$RESP"
echo "rauthy-bootstrap: done"
