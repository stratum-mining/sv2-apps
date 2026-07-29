#!/usr/bin/env bash
set -euo pipefail

# Prints watched JDC/tProxy config example files whose TOML schema changed
# between two git refs.
#
# The comparison intentionally ignores value changes and tracks the structure
# that sv2-ui needs to generate config forms:
#   - table vs array-of-tables, e.g. [upstreams] vs [[upstreams]]
#   - scalar types, e.g. integer vs string
#   - scalar vs array values
#
# This script only reads real TOML config examples. Docker services are
# configured entirely from environment variables and have no config files.

if [[ "$#" -ne 2 ]]; then
  echo "usage: $0 <before-ref> <after-ref>" >&2
  exit 2
fi

BEFORE_REF="$1"
AFTER_REF="$2"

is_watched_config_file() {
  case "$1" in
    miner-apps/jd-client/config-examples/*.toml) return 0 ;;
    miner-apps/translator/config-examples/*.toml) return 0 ;;
    *) return 1 ;;
  esac
}

extract_schema_from_file() {
  local file="$1"

  # Use Python's standard TOML parser so the signature follows TOML semantics
  # instead of approximating table shape with shell text parsing.
  python3 - "$file" <<'PY' | sort -u
import datetime
import sys
import tomllib


def type_name(value):
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, int):
        return "integer"
    if isinstance(value, float):
        return "float"
    if isinstance(value, str):
        return "string"
    if isinstance(value, datetime.datetime):
        return "datetime"
    if isinstance(value, datetime.date):
        return "date"
    if isinstance(value, datetime.time):
        return "time"
    if isinstance(value, list):
        if not value:
            return "array<empty>"
        inner_types = sorted({type_name(item) for item in value})
        return f"array<{','.join(inner_types)}>"
    if isinstance(value, dict):
        return "table"
    return type(value).__name__


def is_array_of_tables(value):
    return bool(value) and isinstance(value, list) and all(isinstance(item, dict) for item in value)


def walk(value, path, emit_table=True):
    if isinstance(value, dict):
        if path and emit_table:
            print(f"table:{'.'.join(path)}")
        for key in sorted(value):
            walk(value[key], [*path, key])
        return

    if is_array_of_tables(value):
        # TOML [[array-of-tables]] parses as a list of dictionaries. Emit the
        # container shape, then merge child field signatures across entries.
        print(f"array-table:{'.'.join(path)}")
        for item in value:
            walk(item, path, emit_table=False)
        return

    print(f"{type_name(value)}:{'.'.join(path)}")


with open(sys.argv[1], "rb") as toml_file:
    walk(tomllib.load(toml_file), [])
PY
}

extract_schema_from_ref() {
  local ref="$1"
  local file="$2"
  local tmp_file

  if ! git cat-file -e "$ref:$file" 2>/dev/null; then
    return 0
  fi

  tmp_file="$(mktemp)"
  git show "$ref:$file" > "$tmp_file"
  if ! extract_schema_from_file "$tmp_file"; then
    rm -f "$tmp_file"
    return 1
  fi
  rm -f "$tmp_file"
}

git diff --name-only "$BEFORE_REF" "$AFTER_REF" -- \
  miner-apps/jd-client/config-examples \
  miner-apps/translator/config-examples |
while IFS= read -r file; do
  if ! is_watched_config_file "$file"; then
    continue
  fi

  if ! diff -q \
    <(extract_schema_from_ref "$BEFORE_REF" "$file") \
    <(extract_schema_from_ref "$AFTER_REF" "$file") >/dev/null; then
    echo "$file"
  fi
done
