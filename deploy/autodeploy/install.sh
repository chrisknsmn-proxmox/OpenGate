#!/usr/bin/env bash
# Installs the OpenGate auto-deploy timer on the Coolify server. Run as root:
#   sudo ./install.sh                 # asks for the Deploy Webhook URL and token
#   sudo ./install.sh --uninstall     # stops it and removes everything it installed
set -euo pipefail
[[ $(id -u) == 0 ]] || { echo "run with sudo" >&2; exit 1; }
here=$(cd "$(dirname "$0")" && pwd)
etc=/etc/opengate-autodeploy

if [[ ${1:-} == --uninstall ]]; then
  systemctl disable --now opengate-autodeploy.timer 2>/dev/null || true
  rm -f /etc/systemd/system/opengate-autodeploy.{service,timer} /usr/local/bin/opengate-autodeploy
  rm -rf "$etc" /var/lib/private/opengate-autodeploy /var/lib/opengate-autodeploy
  systemctl daemon-reload
  echo "removed. Also delete the API token in Coolify (Keys & Tokens)."
  exit 0
fi

for cmd in curl systemctl; do
  command -v "$cmd" >/dev/null || { echo "missing $cmd" >&2; exit 1; }
done

install -m 0755 "$here/opengate-autodeploy.sh" /usr/local/bin/opengate-autodeploy
install -m 0644 "$here/opengate-autodeploy.service" "$here/opengate-autodeploy.timer" /etc/systemd/system/
install -d -m 0700 "$etc"

if [[ ! -f $etc/config ]]; then
  read -rp "Coolify Deploy Webhook URL (Coolify → OpenGate → Webhooks): " url
  [[ $url == http*://*/api/v1/deploy\?* ]] || { echo "that does not look like a Deploy Webhook URL" >&2; exit 1; }
  printf 'COOLIFY_DEPLOY_URL=%s\nREPO=chrisknsmn-proxmox/OpenGate\nBRANCH=main\n' "$url" > "$etc/config"
  chmod 0600 "$etc/config"
fi
if [[ ! -s $etc/coolify-token ]]; then
  read -rsp "Coolify API token (permission: deploy; input hidden): " token; echo
  [[ -n $token ]] || { echo "empty token" >&2; exit 1; }
  (umask 077; printf '%s\n' "$token" > "$etc/coolify-token")
  unset token
fi

systemctl daemon-reload
systemctl enable --now opengate-autodeploy.timer
systemctl start opengate-autodeploy.service   # first run records the baseline commit
journalctl -u opengate-autodeploy -n 3 --no-pager -o cat
echo
echo "installed. Follow it with: journalctl -u opengate-autodeploy -f"
