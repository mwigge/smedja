#!/usr/bin/env bash
# Import the Smedja dashboards from ./dashboards into a running SigNoz.
#
# Usage: ./import-dashboards.sh <email> <password> [signoz-url] [org-id]
#
# email/password: the account created on first launch (the root user).
# org-id: optional — required by SigNoz builds that insist on it at login.
# Find it with:
#   docker cp signoz:/var/lib/signoz/signoz.db /tmp/signoz.db
#   sqlite3 /tmp/signoz.db 'SELECT org_id FROM users;'
#
# Nothing is stored; the session token lives only for this run.
set -euo pipefail

EMAIL="${1:?usage: import-dashboards.sh <email> <password> [signoz-url] [org-id]}"
PASSWORD="${2:?usage: import-dashboards.sh <email> <password> [signoz-url] [org-id]}"
BASE="${3:-http://localhost:8080}"
ORG_ID="${4:-${SIGNOZ_ORG_ID:-}}"
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

curl -sf "$BASE/api/v1/version" >/dev/null || {
  echo "SigNoz not reachable at $BASE" >&2
  exit 1
}

PAYLOAD="{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\""
[[ -n "$ORG_ID" ]] && PAYLOAD="$PAYLOAD,\"orgID\":\"$ORG_ID\""
PAYLOAD="$PAYLOAD}"

RESP=$(curl -s -X POST "$BASE/api/v2/sessions/email_password" \
  -H 'Content-Type: application/json' \
  -d "$PAYLOAD")

if ! echo "$RESP" | grep -q '"accessToken"'; then
  echo "Login failed: $RESP" >&2
  echo "If it says orgID is required, pass it as the 4th argument (see header comment)." >&2
  exit 1
fi

TOKEN=$(echo "$RESP" | python3 -c "import json,sys; print(json.load(sys.stdin)['data']['accessToken'])")

for f in "$DIR"/dashboards/*.json; do
  TITLE=$(python3 -c "import json; print(json.load(open('$f'))['title'])")
  OUT=$(curl -sf -X POST "$BASE/api/v1/dashboards" \
    -H "Authorization: Bearer $TOKEN" \
    -H 'Content-Type: application/json' \
    --data-binary "@$f")
  ID=$(echo "$OUT" | python3 -c "import json,sys; print(json.load(sys.stdin)['data']['id'])")
  echo "imported: $TITLE -> $BASE/dashboard/$ID"
done
