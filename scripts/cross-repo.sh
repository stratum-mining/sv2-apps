#!/usr/bin/env bash

# Helpers for working on sv2-apps against a local stratum repository.
# Source this file to make the functions available in the current shell:
#
#   source scripts/cross-repo.sh

if [ -n "${BASH_VERSION:-}" ]; then
  _CROSS_REPO_SCRIPT_PATH="${BASH_SOURCE[0]}"
elif [ -n "${ZSH_VERSION:-}" ]; then
  _CROSS_REPO_SCRIPT_PATH="${(%):-%N}"
else
  echo "cross-repo.sh: only bash and zsh are supported" >&2
  return 1 2>/dev/null || exit 1
fi

_CROSS_REPO_SCRIPT_DIR=$(cd "$(dirname "$_CROSS_REPO_SCRIPT_PATH")" 2>/dev/null && pwd -P)
: "${CROSS_REPO_ROOT:=$(dirname "$_CROSS_REPO_SCRIPT_DIR")}"
: "${CROSS_REPO_STRATUM_SYMLINK:=stratum}"

_cross_repo_cargo_tomls=(
  "stratum-apps/Cargo.toml"
  "pool-apps/Cargo.toml"
  "miner-apps/Cargo.toml"
  "integration-tests/Cargo.toml"
  "bitcoin-core-sv2/Cargo.toml"
)

_cross_repo_realpath() {
  local path="$1"

  case "$path" in
    "~")
      path="$HOME"
      ;;
    "~/"*)
      path="$HOME/${path#~/}"
      ;;
  esac

  if command -v realpath >/dev/null 2>&1; then
    realpath "$path" 2>/dev/null
    return $?
  fi

  if [ -d "$path" ]; then
    (cd "$path" 2>/dev/null && pwd -P)
    return $?
  fi

  local dir base
  dir=$(dirname "$path")
  base=$(basename "$path")
  dir=$(cd "$dir" 2>/dev/null && pwd -P) || return 1
  printf '%s/%s\n' "$dir" "$base"
}

_cross_repo_symlink_target() {
  local link_path="$1"
  local target

  target=$(readlink "$link_path" 2>/dev/null) || return 1
  case "$target" in
    /*)
      _cross_repo_realpath "$target"
      ;;
    *)
      _cross_repo_realpath "$(dirname "$link_path")/$target"
      ;;
  esac
}

_cross_repo_update_stratum_symlink() {
  local repo_path="$1"
  local link_path="$CROSS_REPO_ROOT/$CROSS_REPO_STRATUM_SYMLINK"
  local current_target

  if [ -L "$link_path" ]; then
    current_target=$(_cross_repo_symlink_target "$link_path" 2>/dev/null)
    if [ "$current_target" = "$repo_path" ]; then
      return 0
    fi

    rm -f "$link_path" || return 1
    ln -s "$repo_path" "$link_path" || return 1
    echo "Updated symlink $CROSS_REPO_STRATUM_SYMLINK -> $repo_path" >&2
    return 0
  fi

  if [ -e "$link_path" ]; then
    echo "Error: $link_path already exists and is not a symlink" >&2
    return 1
  fi

  ln -s "$repo_path" "$link_path" || return 1
  echo "Created symlink $CROSS_REPO_STRATUM_SYMLINK -> $repo_path" >&2
}

get-stratum-core-path() {
  local stratum_repo_path="${1:-}"
  local stratum_core_path
  local link_path="$CROSS_REPO_ROOT/$CROSS_REPO_STRATUM_SYMLINK"

  if [ -z "$stratum_repo_path" ] && [ -L "$link_path" ]; then
    stratum_repo_path=$(_cross_repo_symlink_target "$link_path" 2>/dev/null)
    stratum_core_path="$stratum_repo_path/stratum-core"
    if [ -d "$stratum_core_path" ]; then
      printf '%s\n' "$stratum_core_path"
      return 0
    fi

    echo "Warning: stratum-core not found at $stratum_core_path, will prompt for stratum repository path" >&2
    stratum_repo_path=""
  fi

  if [ -z "$stratum_repo_path" ]; then
    echo "Please provide the local path to the stratum repository:" >&2
    read -r stratum_repo_path
  fi

  if [ -z "$stratum_repo_path" ]; then
    echo "Error: Path cannot be empty" >&2
    return 1
  fi

  stratum_repo_path=$(_cross_repo_realpath "$stratum_repo_path" 2>/dev/null)
  if [ -z "$stratum_repo_path" ] || [ ! -d "$stratum_repo_path" ]; then
    echo "Error: Directory does not exist: ${1:-$stratum_repo_path}" >&2
    return 1
  fi

  stratum_core_path="$stratum_repo_path/stratum-core"
  if [ ! -d "$stratum_core_path" ]; then
    echo "Error: stratum-core directory not found at: $stratum_core_path" >&2
    echo "Please provide the path to the stratum repository root (which contains stratum-core as a subdirectory)" >&2
    return 1
  fi

  _cross_repo_update_stratum_symlink "$stratum_repo_path" || return 1
  printf '%s\n' "$stratum_core_path"
}

_cross_repo_write_if_changed() {
  local tmp_file="$1"
  local dest_file="$2"

  if cmp -s "$tmp_file" "$dest_file"; then
    rm -f "$tmp_file"
  else
    mv "$tmp_file" "$dest_file"
  fi
}

_cross_repo_upsert_patch_section() {
  local cargo_toml="$1"
  local stratum_core_abs="$2"
  local tmp_file

  tmp_file=$(mktemp "${cargo_toml}.XXXXXX") || return 1

  if grep -q '^\[patch\."https://github.com/stratum-mining/stratum"\]' "$cargo_toml"; then
    awk -v path="$stratum_core_abs" '
      /^\[patch\."https:\/\/github.com\/stratum-mining\/stratum"\]/ {
        print
        print "stratum-core = {path = \"" path "\"}"
        in_target = 1
        next
      }
      in_target && /^\[/ {
        in_target = 0
        print
        next
      }
      in_target && /^stratum-core = \{path =/ {
        next
      }
      {
        print
      }
    ' "$cargo_toml" > "$tmp_file" || {
      rm -f "$tmp_file"
      return 1
    }
    _cross_repo_write_if_changed "$tmp_file" "$cargo_toml"
    echo "Updated [patch.\"https://github.com/stratum-mining/stratum\"] section in $cargo_toml" >&2
  else
    cp "$cargo_toml" "$tmp_file" || {
      rm -f "$tmp_file"
      return 1
    }
    {
      printf '\n'
      printf '# Override dependencies with local paths\n'
      printf '[patch."https://github.com/stratum-mining/stratum"]\n'
      printf 'stratum-core = {path = "%s"}\n' "$stratum_core_abs"
    } >> "$tmp_file" || {
      rm -f "$tmp_file"
      return 1
    }
    _cross_repo_write_if_changed "$tmp_file" "$cargo_toml"
    echo "Added [patch.\"https://github.com/stratum-mining/stratum\"] section to $cargo_toml" >&2
  fi
}

patch-cargo-toml() {
  local stratum_core_path stratum_core_abs cargo_toml

  stratum_core_path=$(get-stratum-core-path "$@") || return 1
  stratum_core_abs=$(_cross_repo_realpath "$stratum_core_path" 2>/dev/null)
  if [ -z "$stratum_core_abs" ] || [ ! -d "$stratum_core_abs" ]; then
    echo "Error: Failed to resolve absolute path for stratum-core: $stratum_core_path" >&2
    return 1
  fi

  for cargo_toml in "${_cross_repo_cargo_tomls[@]}"; do
    cargo_toml="$CROSS_REPO_ROOT/$cargo_toml"
    [ -f "$cargo_toml" ] || continue
    _cross_repo_upsert_patch_section "$cargo_toml" "$stratum_core_abs" || return 1
  done

  echo "Patched all Cargo.toml files to use local stratum-core at: $stratum_core_abs" >&2
}

_cross_repo_remove_patch_section() {
  local cargo_toml="$1"
  local tmp_file

  tmp_file=$(mktemp "${cargo_toml}.XXXXXX") || return 1

  awk '
    function flush_pending_comment() {
      if (pending_comment != "") {
        print pending_comment
        pending_comment = ""
      }
    }

    function flush_target_section(  i, has_content) {
      has_content = 0
      for (i = 1; i <= target_count; i++) {
        if (target_lines[i] !~ /^[[:space:]]*$/ && target_lines[i] !~ /^[[:space:]]*#/) {
          has_content = 1
        }
      }

      if (has_content) {
        flush_pending_comment()
        print target_header
        for (i = 1; i <= target_count; i++) {
          print target_lines[i]
        }
      } else {
        pending_comment = ""
      }

      in_target = 0
      target_count = 0
      target_header = ""
    }

    /^# Override dependencies with local paths$/ {
      pending_comment = $0
      next
    }

    /^\[patch\."https:\/\/github.com\/stratum-mining\/stratum"\]/ {
      if (in_target) {
        flush_target_section()
      }
      in_target = 1
      target_header = $0
      target_count = 0
      next
    }

    in_target && /^\[/ {
      flush_target_section()
      flush_pending_comment()
      print
      next
    }

    in_target {
      if ($0 ~ /^stratum-core = \{path =/) {
        next
      }
      target_lines[++target_count] = $0
      next
    }

    {
      flush_pending_comment()
      print
    }

    END {
      if (in_target) {
        flush_target_section()
      } else {
        flush_pending_comment()
      }
    }
  ' "$cargo_toml" > "$tmp_file" || {
    rm -f "$tmp_file"
    return 1
  }

  _cross_repo_write_if_changed "$tmp_file" "$cargo_toml"
}

_cross_repo_trim_trailing_blank_lines() {
  local cargo_toml="$1"
  local tmp_file

  tmp_file=$(mktemp "${cargo_toml}.XXXXXX") || return 1
  awk '
    {
      lines[NR] = $0
    }
    END {
      n = NR
      while (n > 0 && lines[n] ~ /^[[:space:]]*$/) {
        n--
      }
      for (i = 1; i <= n; i++) {
        print lines[i]
      }
    }
  ' "$cargo_toml" > "$tmp_file" || {
    rm -f "$tmp_file"
    return 1
  }

  _cross_repo_write_if_changed "$tmp_file" "$cargo_toml"
}

restore-cargo-toml() {
  local cargo_toml cargo_dir cargo_lock

  for cargo_toml in "${_cross_repo_cargo_tomls[@]}"; do
    cargo_toml="$CROSS_REPO_ROOT/$cargo_toml"
    [ -f "$cargo_toml" ] || continue

    if grep -q '^\[patch\."https://github.com/stratum-mining/stratum"\]' "$cargo_toml"; then
      _cross_repo_remove_patch_section "$cargo_toml" || return 1
      _cross_repo_trim_trailing_blank_lines "$cargo_toml" || return 1
      echo "Restored GitHub stratum-core dependency in $cargo_toml" >&2
    else
      echo "No [patch.\"https://github.com/stratum-mining/stratum\"] section found in $cargo_toml" >&2
    fi

    cargo_dir=$(dirname "$cargo_toml")
    cargo_lock="$cargo_dir/Cargo.lock"
    if [ -f "$cargo_lock" ]; then
      if command -v cargo >/dev/null 2>&1; then
        cargo update -w --manifest-path "$cargo_toml" || return 1
        echo "Refreshed lockfile at $cargo_lock" >&2
      else
        echo "Warning: cargo is not installed; lockfile not refreshed: $cargo_lock" >&2
      fi
    fi
  done
}

cross-repo-help() {
  echo "Available commands:"
  echo "  Cargo.toml management:"
  echo "    patch-cargo-toml      - Patch/update Cargo.toml to use local stratum repository"
  echo "    restore-cargo-toml    - Remove patch section, restore GitHub dependency"
  echo "    get-stratum-core-path - Get or set the stratum repository path (creates/updates symlink)"
  echo ""
  echo "Usage:"
  echo "  source scripts/cross-repo.sh"
  echo "  patch-cargo-toml"
  echo "  patch-cargo-toml /path/to/stratum"
  echo "  restore-cargo-toml"
  echo "  get-stratum-core-path"
  echo "  get-stratum-core-path /path/to/stratum"
}

_cross_repo_sourced=0
if [ -n "${ZSH_VERSION:-}" ]; then
  case "${ZSH_EVAL_CONTEXT:-}" in
    *:file|*:file:*)
      _cross_repo_sourced=1
      ;;
  esac
elif [ -n "${BASH_VERSION:-}" ]; then
  if [ "${BASH_SOURCE[0]}" != "$0" ]; then
    _cross_repo_sourced=1
  fi
fi

if [ "$_cross_repo_sourced" -eq 0 ]; then
  cross-repo-help
fi

