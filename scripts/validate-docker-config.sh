#!/usr/bin/env bash
set -euo pipefail


CONFIG_FILES=(
  "pool-apps/pool/config-examples/mainnet/pool-jds-config-bitcoin-core-ipc-example.toml"
  "miner-apps/jd-client/config-examples/mainnet/jdc-config-bitcoin-core-ipc-hosted-infra-example.toml"
  "miner-apps/translator/config-examples/mainnet/tproxy-config-local-jdc-example.toml"
)

get_template_for_config() {
  case "$1" in
    pool-apps/pool/config-examples/mainnet/pool-jds-config-bitcoin-core-ipc-example.toml)
      echo "docker/config/pool-jds-config.toml.template"
      ;;

    miner-apps/jd-client/config-examples/mainnet/jdc-config-bitcoin-core-ipc-hosted-infra-example.toml)
      echo "docker/config/jdc-config.toml.template"
      ;;
    miner-apps/translator/config-examples/mainnet/tproxy-config-local-jdc-example.toml)
      echo "docker/config/translator-proxy-config.toml.template"
      ;;
    *)
      echo ""
      ;;
  esac
}

# WARNING: Implemented with GPT-5
#
# Extracts key names from a TOML-like file.
#
# Behavior:
#   - Ignores comments and blank lines
#   - Tracks the current [section]
#   - For lines containing '=', treats the left-hand side as a key
#   - Outputs keys as "section.key" or "key" if no section is active
#   - Sorts and removes duplicates
#
# Limitations:
#   - Not a full TOML parser
#   - Does not handle quoted keys, inline tables, arrays, multiline strings,
#     escapes, or nested sections
extract_keys() {
  local file="$1"

  awk '
  /^[[:space:]]*#/ { next }
  /^[[:space:]]*$/ { next }

  /^\[.*\]$/ {
    section=$0
    gsub(/[\[\]]/, "", section)
    next
  }

  /=/ {
    split($0, a, "=")
    key=a[1]
    gsub(/[[:space:]]+$/, "", key)

    if (section != "")
      print section "." key
    else
      print key
  }
' "$file" | sort -u
}

echo "Validating TOML structure against Docker templates"
echo

FAILED=0

for CONFIG_FILE in "${CONFIG_FILES[@]}"; do
  TEMPLATE_FILE="$(get_template_for_config "$CONFIG_FILE")"

  if [[ -z "$TEMPLATE_FILE" ]]; then
    echo "❌ No Docker template mapped for $CONFIG_FILE"
    FAILED=1
    continue
  fi

  if [[ ! -f "$CONFIG_FILE" ]]; then
    echo "⚠️ Missing config file: $CONFIG_FILE"
    FAILED=1
    continue
  fi

  if [[ ! -f "$TEMPLATE_FILE" ]]; then
    echo "❌ Missing Docker template: $TEMPLATE_FILE"
    FAILED=1
    continue
  fi

  echo "▶ Comparing"
  echo "   Config:   $CONFIG_FILE"
  echo "   Template: $TEMPLATE_FILE"

  CONFIG_KEYS=$(extract_keys "$CONFIG_FILE")
  TEMPLATE_KEYS=$(extract_keys "$TEMPLATE_FILE")

DIFF=$(diff -u -U0 <(echo "$TEMPLATE_KEYS") <(echo "$CONFIG_KEYS") \
  | grep -E '^[+-]' || true)

  if [[ -n "$DIFF" ]]; then
    echo
    echo "❌ Config / Docker drift detected"
    echo
    echo "$DIFF"
    echo
    FAILED=1
  else
    echo "✅ In sync"
    echo
  fi
done

if [[ "$FAILED" -ne 0 ]]; then
  echo "🚨 Docker templates are out of sync with config examples."
  echo "👉 Update the Docker templates to reflect all config keys."
  exit 1
fi

echo "All Docker templates match their configs."
