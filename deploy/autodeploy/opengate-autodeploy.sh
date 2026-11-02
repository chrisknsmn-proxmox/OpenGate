#!/usr/bin/env bash
# Deploys new OpenGate commits to Coolify (README: "Automatic deployment").
#
# Runs on the Coolify server from opengate-autodeploy.timer. Each run reads the
# tip of $BRANCH from GitHub over outbound HTTPS and, if it changed since the
# last successful request, asks Coolify to deploy through its Deploy Webhook on
# the LAN. Nothing is exposed to the Internet and GitHub holds no secrets.
#
# Config (EnvironmentFile /etc/opengate-autodeploy/config):
#   COOLIFY_DEPLOY_URL   Coolify → application → Webhooks → "Deploy Webhook"
#   REPO, BRANCH         default chrisknsmn-proxmox/OpenGate, main
# Secret (systemd credential from /etc/opengate-autodeploy/coolify-token):
#   a Coolify API token with only the `deploy` permission. Never logged.
set -euo pipefail

REPO=${REPO:-chrisknsmn-proxmox/OpenGate}
BRANCH=${BRANCH:-main}
: "${COOLIFY_DEPLOY_URL:?COOLIFY_DEPLOY_URL is not set (see /etc/opengate-autodeploy/config)}"
token_file=${CREDENTIALS_DIRECTORY:-/etc/opengate-autodeploy}/coolify-token
state_dir=${STATE_DIRECTORY:-/var/lib/opengate-autodeploy}
state="$state_dir/deployed-sha"

# Tip of the branch from Git's smart-HTTP ref list: needs only curl, and is not
# subject to the REST API's 60 requests/hour limit for anonymous clients.
sha=$(curl -fsS --max-time 20 \
        "https://github.com/$REPO.git/info/refs?service=git-upload-pack" \
      | grep -a -oE "[0-9a-f]{40} refs/heads/$BRANCH\$" | cut -c1-40 | head -n1) || true
if [[ ! $sha =~ ^[0-9a-f]{40}$ ]]; then
  echo "could not read $REPO $BRANCH from GitHub; retrying next run" >&2
  exit 1
fi

last=$(cat "$state" 2>/dev/null || true)
if [[ -z $last ]]; then
  # First run: remember what is there now instead of redeploying on install.
  echo "$sha" > "$state"
  echo "baseline: $BRANCH is at ${sha:0:7}; the next new commit will deploy"
  exit 0
fi
[[ $sha == "$last" ]] && exit 0

echo "new commit on $BRANCH: ${last:0:7} -> ${sha:0:7}; requesting a Coolify deployment"
token=$(tr -d '\r\n' < "$token_file")
# The token goes to curl on stdin (-K -), so it never appears in argv.
resp=$(printf 'header = "Authorization: Bearer %s"\n' "$token" \
        | curl -sS --max-time 30 -K - -X POST -w '\n%{http_code}' "$COOLIFY_DEPLOY_URL") || {
  echo "cannot reach Coolify at the deploy URL; retrying next run" >&2
  exit 1
}
code=${resp##*$'\n'}
body=${resp%$'\n'*}
if [[ $code == 2?? ]]; then
  echo "$sha" > "$state"
  echo "Coolify accepted the deployment of ${sha:0:7}: ${body:0:300}"
else
  echo "Coolify answered HTTP $code: ${body:0:300}; retrying next run" >&2
  exit 1
fi
