#!/usr/bin/env bash
# Secret guard for this repository. Run by .githooks/pre-commit (staged
# changes) and .githooks/pre-push (every commit about to be pushed).
#
# Enable once per clone:   git config core.hooksPath .githooks
# Scan by hand:            .githooks/scan-secrets.sh staged
#                          .githooks/scan-secrets.sh range <rev-range>
#
# It blocks:
#   1. Secret-bearing files (.env, keys, credential stores) being added.
#   2. Any value from your local .env / .env.* appearing in added lines,
#      including the host of URL values and the vendor prefix of MACs.
#      Values that also appear in .env.example (public defaults) are ignored.
#   3. Common credential formats (private keys, OpenAI/GitHub/AWS/Slack/Google
#      tokens, JWTs, long literal secrets assigned to key/token/password names).
#
# Findings are reported by file, line and kind; secret values are never printed.
#
# False positive? Add `opengate:allow-secret` to that line, or list the exact
# path / literal string in .githooks/secret-allowlist (and review it in the PR).
set -euo pipefail

g() { command grep "$@"; }

root=$(git rev-parse --show-toplevel)
cd "$root"
allowlist="$root/.githooks/secret-allowlist"

work=$(mktemp -d)
chmod 700 "$work"
trap 'rm -rf "$work"' EXIT

mode=${1:-staged}
case "$mode" in
  staged)
    git diff --cached --name-only --diff-filter=ACR -z | tr '\0' '\n' > "$work/paths"
    git diff --cached -U0 --no-color --no-ext-diff --diff-filter=ACMR > "$work/diff"
    ;;
  range)
    range=${2:?usage: scan-secrets.sh range <rev-range>}
    # shellcheck disable=SC2086 # a range may be several revision arguments
    git log --format= --name-only --diff-filter=ACR $range | g -v '^$' | sort -u > "$work/paths" || true
    # shellcheck disable=SC2086
    git log -p -U0 --no-color --no-ext-diff --format='commit %h' --diff-filter=ACMR $range > "$work/diff"
    ;;
  *) echo "usage: scan-secrets.sh staged | range <rev-range>" >&2; exit 2 ;;
esac

# Allowlist: `path:<exact path>` lines and `literal:<string>` lines.
: > "$work/allow_paths"; : > "$work/allow_literals"
if [[ -f $allowlist ]]; then
  sed -n 's/^path://p' "$allowlist" > "$work/allow_paths"
  sed -n 's/^literal://p' "$allowlist" > "$work/allow_literals"
fi

# Added lines as "commit<US>file<US>line<US>text" (US = \037, never whitespace,
# so an empty commit field cannot shift the others and expose the text).
awk -v OFS='\037' '
  /^commit [0-9a-f]+$/ { c = $2; next }
  /^\+\+\+ /           { f = substr($0, 5); sub(/^b\//, "", f); next }
  /^@@ /               { split($3, a, ","); n = substr(a[1], 2) + 0; next }
  /^\+/                { print c, f, n, substr($0, 2); n++ }
' "$work/diff" > "$work/added_all"

# Drop allowlisted paths, marked lines and allowlisted literals.
awk -F'\037' 'NR == FNR { if ($0 != "") skip[$0] = 1; next } !($2 in skip)' \
  "$work/allow_paths" "$work/added_all" \
  | { g -vF 'opengate:allow-secret' || true; } > "$work/added"
if [[ -s $work/allow_literals ]]; then
  { g -vF -f "$work/allow_literals" "$work/added" || true; } > "$work/added.tmp"
  mv "$work/added.tmp" "$work/added"
fi

# Findings go to a file: report often runs in a pipeline subshell.
findings="$work/findings"; : > "$findings"
report() { # kind, matching records on stdin
  local kind=$1
  while IFS=$'\037' read -r c f n _; do
    printf '  %-44s %s:%s%s\n' "$kind" "$f" "$n" "${c:+ (commit $c)}" >> "$findings"
  done
}

# 1. Secret-bearing files.
forbidden='(^|/)\.env(\..*)?$|\.(pem|key|p12|pfx|jks|keystore|kdbx|ppk|asc|gpg)$|(^|/)id_(rsa|dsa|ecdsa|ed25519)|(^|/)credentials[^/]*\.json$|(^|/)\.(netrc|pgpass|npmrc|pypirc)$|(^|/)\.docker/config\.json$'
while IFS= read -r p; do
  [[ -z $p || $p == .env.example || $p == */.env.example ]] && continue
  g -qxF -- "$p" "$work/allow_paths" && continue
  if g -qE -- "$forbidden" <<< "$p"; then
    printf '  %-44s %s\n' "secret-bearing file" "$p" >> "$findings"
  fi
done < "$work/paths"

# 2. Real values from local env files.
example_values=$(sed -n 's/^[A-Za-z_][A-Za-z0-9_]*=//p' .env.example 2>/dev/null || true)
check_value() { # label, value
  local label=$1 v=$2
  [[ ${#v} -lt 6 ]] && return 0
  [[ $v =~ ^[0-9]+(\.[0-9]+)?$ || $v =~ ^(true|false|info|debug|warn|error)$ ]] && return 0
  printf '%s\n' "$example_values" | g -qxF -- "$v" && return 0
  printf '%s\n' "$v" > "$work/needle"
  g -iF -f "$work/needle" "$work/added" | report "value of $label from .env" || true
}
for envfile in .env .env.*; do
  [[ -f $envfile && $envfile != .env.example ]] || continue
  while IFS= read -r line || [[ -n $line ]]; do
    [[ $line =~ ^[[:space:]]*(export[[:space:]]+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]] || continue
    k=${BASH_REMATCH[2]} v=${BASH_REMATCH[3]}
    v=${v%%[[:space:]]#*}; v=${v%\"}; v=${v#\"}; v=${v%\'}; v=${v#\'}
    [[ -z $v ]] && continue
    check_value "$k" "$v"
    if [[ $v =~ ://([^/:@]+@)?([^/:]+) ]]; then
      check_value "$k (host)" "${BASH_REMATCH[2]}"
    fi
    if [[ $v =~ ^([0-9A-Fa-f]{2})[:-]([0-9A-Fa-f]{2})[:-]([0-9A-Fa-f]{2})[:-]([0-9A-Fa-f]{2})[:-]([0-9A-Fa-f]{2})[:-]([0-9A-Fa-f]{2})$ ]]; then
      b=("${BASH_REMATCH[@]:1}")
      check_value "$k" "$(IFS=:; echo "${b[*]}")"
      check_value "$k" "$(IFS=-; echo "${b[*]}")"
      check_value "$k (vendor prefix)" "${b[0]}:${b[1]}:${b[2]}"
      check_value "$k (vendor prefix)" "${b[0]}-${b[1]}-${b[2]}"
      check_value "$k (prefix)" "${b[0]}:${b[1]}:"
      check_value "$k (prefix)" "${b[0]}-${b[1]}-"
    fi
  done < "$envfile"
done

# 3. Known credential formats.
patterns=(
  'private key|-----BEGIN ([A-Z]+ )?PRIVATE KEY'
  'OpenAI key|sk-(proj-|svcacct-|admin-)?[A-Za-z0-9_-]{20,}'
  'Anthropic key|sk-ant-[A-Za-z0-9_-]{20,}'
  'GitHub token|(gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{40,})'
  'AWS access key|(AKIA|ASIA)[0-9A-Z]{16}'
  'Slack token|xox[abposr]-[A-Za-z0-9-]{10,}'
  'Google API key|AIza[0-9A-Za-z_-]{35}'
  'JWT|eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}'
  'URL with password|[a-z][a-z0-9+.-]*://[^/[:space:]:@]+:[^/[:space:]@]{3,}@'
  'hard-coded secret|(api[_-]?key|secret|token|passw(or)?d|credentials?[_-]?key)[A-Za-z_]*["'"'"']?[[:space:]]*[:=][[:space:]]*["'"'"']?[A-Za-z0-9+/_=-]{24,}'
)
# Obvious placeholders are not secrets (only for the generic assignment rule).
placeholder='replace[-_]?(me|with)|change[-_]?me|example|placeholder|your[-_]|dummy|fake|xxxx|<[a-z_-]+>'
for entry in "${patterns[@]}"; do
  kind=${entry%%|*} re=${entry#*|}
  awk -F'\037' '{ print $4 }' "$work/added" > "$work/texts"
  g -nE -i -- "$re" "$work/texts" \
    | { if [[ $kind == "hard-coded secret" ]]; then g -viE "$placeholder" || true; else cat; fi; } \
    | cut -d: -f1 | while read -r i; do
    sed -n "${i}p" "$work/added"
  done | report "$kind" || true
done

if [[ -s $findings ]]; then
  sort -u "$findings" >&2
  cat >&2 <<'EOF'

Blocked: possible secret or private detail (values are not shown).
Remove it, or if it is a placeholder/test value, mark the line with
`opengate:allow-secret` or add it to .githooks/secret-allowlist.
EOF
  exit 1
fi
