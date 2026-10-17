#!/bin/sh
# Bootstraps Rauthy for the Compose stack (docker-compose.yml):
#   - groups mb:sysadmin and mb:default:admin;
#   - the confidential client `mockingbird`, whose secret is written to
#     /bootstrap/client_secret for Mockingbird to read;
#   - the demo user demo@localhost.test (password Demo-Passw0rd-123!) in both
#     groups.
# Idempotent: every object is looked up before it is created (Rauthy answers a
# duplicate with 400, not 409), so it can run on every `docker compose up`.
# Authenticates with Rauthy's bootstrap API key (`Authorization: API-Key
# <name>$<secret>`), which needs Clients, Groups and Users access plus
# Secrets read/update for the client secret.
#
# Checked against Rauthy 0.37.1 (src/api/src/{groups,clients,users}.rs and
# src/api_types); the Swagger UI is at http://localhost:8081/auth/v1/docs.
# Where a call fails, the script prints the manual steps (also in
# deploy/compose/README.md) and exits non-zero.
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

# expect STATUS WHAT: 2xx passes; anything else prints the manual steps.
expect() {
    case "$1" in
        2??) echo "rauthy-bootstrap: $2: ok ($1)" ;;
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

# Groups: GET /groups lists [{"id","name"}]; POST /groups takes {"group": NAME}.
status="$(call GET /groups)"
expect "$status" "list groups"
existing_groups="$(cat "$RESP")"
for group in mb:sysadmin mb:default:admin; do
    case "$existing_groups" in
        *"\"name\":\"$group\""*)
            echo "rauthy-bootstrap: group $group: already exists"
            continue
            ;;
    esac
    status="$(call POST /groups "{\"group\":\"$group\"}")"
    expect "$status" "group $group"
done

# The confidential client (NewClientRequest). New Rauthy clients allow the
# scopes openid, email, profile and groups and require PKCE S256; the check
# below fails if `groups` is missing.
client_created=false
status="$(call GET "/clients/$CLIENT_ID")"
if [ "$status" = 404 ]; then
    status="$(call POST /clients "{\"id\":\"$CLIENT_ID\",\"name\":\"Mockingbird\",\"confidential\":true,\"redirect_uris\":[\"$CONSOLE/auth/callback\"],\"post_logout_redirect_uris\":[\"$CONSOLE/\"]}")"
    expect "$status" "client $CLIENT_ID"
    client_created=true
    status="$(call GET "/clients/$CLIENT_ID")"
elif [ "$status" = 200 ]; then
    echo "rauthy-bootstrap: client $CLIENT_ID: already exists"
fi
expect "$status" "read client $CLIENT_ID"
if ! grep -q 'groups' "$RESP"; then
    manual_steps "client $CLIENT_ID does not allow the groups scope"
fi

# Its secret: generated when the client is new or the file is missing (a
# rerun keeps the secret Mockingbird already uses).
if [ "$client_created" = true ] || [ ! -s "$OUT_DIR/client_secret" ]; then
    # PUT /clients/{id}/secret (optional ClientSecretRequest) generates a new
    # secret and returns ClientSecretResponse {"id","confidential","secret"}.
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

# The demo user: GET /users/email/{email}, else POST /users (NewUserRequest).
# Both return a UserResponse whose first field is "id".
user_created=false
status="$(call GET "/users/email/$DEMO_EMAIL")"
if [ "$status" = 404 ]; then
    status="$(call POST /users "{\"email\":\"$DEMO_EMAIL\",\"given_name\":\"Demo\",\"family_name\":\"User\",\"language\":\"en\",\"roles\":[],\"groups\":$GROUPS_JSON}")"
    expect "$status" "user $DEMO_EMAIL"
    user_created=true
else
    expect "$status" "look up user $DEMO_EMAIL"
fi
user_id="$(json_field id)"
if [ -z "$user_id" ]; then
    manual_steps "no id for user $DEMO_EMAIL"
fi

# Its groups (on every run; the update replaces them, so a rerun changes
# nothing) and, only when just created, its password: Rauthy refuses to set
# one of the last 3 passwords again, so a rerun leaves it as it is.
# PUT /users/{id} (UpdateUserRequest, whose password is optional).
password_member=""
what="groups of $DEMO_EMAIL"
if [ "$user_created" = true ]; then
    password_member="\"password\":\"$DEMO_PASSWORD\","
    what="password and groups of $DEMO_EMAIL"
fi
status="$(call PUT "/users/$user_id" "{\"email\":\"$DEMO_EMAIL\",\"given_name\":\"Demo\",\"family_name\":\"User\",\"language\":\"en\",$password_member\"roles\":[],\"groups\":$GROUPS_JSON,\"enabled\":true,\"email_verified\":true}")"
expect "$status" "$what"

rm -f "$RESP"
echo "rauthy-bootstrap: done"
