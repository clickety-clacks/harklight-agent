#!/bin/sh
set -u

version=$1
target=$2
expected_archive_sha=$3
remote_archive=$4
release_root="$HOME/.local/share/harklight-agent/releases"
active_link="$HOME/.local/bin/harklight-agent"
stage=""
temporary_link=""
prior_version=""
prior_path=""
prior_sha=""

file_sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        return 1
    fi
}

resolve_active_path() {
    resolved=""
    if [ -L "$active_link" ]; then
        raw_target=$(readlink "$active_link" 2>/dev/null || true)
        case "$raw_target" in
            /*) resolved=$raw_target ;;
            *)
                link_dir=$(cd "$(dirname "$active_link")" 2>/dev/null && pwd)
                [ -n "$link_dir" ] && resolved="$link_dir/$raw_target"
                ;;
        esac
    elif [ -f "$active_link" ]; then
        resolved=$active_link
    fi
}

emit_readback() {
    outcome=$1
    reason=$2
    version_output=""
    active_sha=""
    resolve_active_path
    if [ -x "$active_link" ]; then
        version_output=$("$active_link" --version 2>/dev/null || true)
        active_sha=$(file_sha256 "$active_link" 2>/dev/null || true)
    fi
    printf 'FLEET1\0%s\0%s\0%s\0%s\0%s\0%s\0%s\0%s\0' \
        "$outcome" "$reason" "$version_output" "$resolved" "$active_sha" \
        "$prior_version" "$prior_path" "$prior_sha"
}

cleanup() {
    [ -z "$temporary_link" ] || rm -f "$temporary_link"
    [ -z "$stage" ] || rm -rf "$stage"
    rm -f "$remote_archive"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

fail() {
    emit_readback failed "$1"
    exit 1
}

case "$version" in
    ""|*[!0-9.]*|.*|*.) fail invalid_version ;;
esac
case "$target" in
    aarch64-apple-darwin|x86_64-apple-darwin|x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu) ;;
    *) fail invalid_platform ;;
esac
case "$expected_archive_sha" in
    ""|*[!0-9a-f]*) fail invalid_archive_checksum ;;
esac
archive_sha_length=$(printf '%s' "$expected_archive_sha" | wc -c | tr -d ' ')
if [ "$archive_sha_length" -ne 64 ]; then
    fail invalid_archive_checksum
fi

prior_output=""
if [ -x "$active_link" ]; then
    prior_output=$("$active_link" --version 2>/dev/null || true)
    case "$prior_output" in
        "harklight-agent "*) prior_version=$(printf '%s' "$prior_output" | sed 's/^harklight-agent //') ;;
    esac
    resolve_active_path
    prior_path=$resolved
    prior_sha=$(file_sha256 "$active_link" 2>/dev/null || true)
fi

actual_archive_sha=$(file_sha256 "$remote_archive" 2>/dev/null) || fail checksum_tool_unavailable
if [ "$actual_archive_sha" != "$expected_archive_sha" ]; then
    fail checksum_mismatch
fi

mkdir -p "$release_root" || fail release_root_unavailable
stage=$(mktemp -d "$release_root/.stage.XXXXXX" 2>/dev/null) || fail stage_unavailable
mkdir -p "$stage/extracted" || fail stage_unavailable
tar -xzf "$remote_archive" -C "$stage/extracted" >/dev/null 2>&1 || fail archive_extract_failed
package_dir="$stage/extracted/harklight-agent-$version-$target"
candidate="$package_dir/harklight-agent"
if [ ! -f "$candidate" ] || [ -L "$candidate" ]; then
    fail archive_binary_missing
fi
chmod 755 "$candidate" || fail candidate_not_executable
candidate_output=$(cd "$package_dir" && ./harklight-agent --version 2>&1) || fail candidate_did_not_run
if [ "$candidate_output" != "harklight-agent $version" ]; then
    fail candidate_version_mismatch
fi
candidate_sha=$(file_sha256 "$candidate" 2>/dev/null) || fail checksum_tool_unavailable

release_dir="$release_root/v$version"
release_binary="$release_dir/harklight-agent"
if [ -L "$release_dir" ]; then
    fail target_release_path_conflict
fi
if [ -e "$release_dir" ]; then
    if [ ! -f "$release_binary" ] || [ -L "$release_binary" ]; then
        fail target_release_path_conflict
    fi
    existing_sha=$(file_sha256 "$release_binary" 2>/dev/null) || fail checksum_tool_unavailable
    existing_output=$("$release_binary" --version 2>/dev/null || true)
    if [ "$existing_sha" != "$candidate_sha" ] || [ "$existing_output" != "harklight-agent $version" ]; then
        fail target_release_path_conflict
    fi
else
    mv "$candidate" "$stage/harklight-agent" || fail candidate_staging_failed
    rm -rf "$stage/extracted"
    if ! mv "$stage" "$release_dir"; then
        fail release_directory_install_failed
    fi
    stage=""
fi

final_output=$("$release_binary" --version 2>&1) || fail installed_binary_did_not_run
if [ "$final_output" != "harklight-agent $version" ]; then
    fail installed_binary_version_mismatch
fi
final_sha=$(file_sha256 "$release_binary" 2>/dev/null) || fail checksum_tool_unavailable
if [ "$final_sha" != "$candidate_sha" ]; then
    fail installed_binary_checksum_mismatch
fi

resolve_active_path
current_path=$resolved
if [ "$current_path" = "$release_binary" ] && [ "$prior_sha" = "$candidate_sha" ] && [ "$prior_output" = "$final_output" ]; then
    outcome=already_current
else
    mkdir -p "$(dirname "$active_link")" || fail active_directory_unavailable
    temporary_link="$active_link.tmp.$$"
    rm -f "$temporary_link"
    ln -s "$release_binary" "$temporary_link" || fail active_link_create_failed
    mv -f "$temporary_link" "$active_link" || fail active_link_switch_failed
    temporary_link=""
    active_output=$("$active_link" --version 2>/dev/null || true)
    active_sha=$(file_sha256 "$active_link" 2>/dev/null || true)
    if [ "$active_output" != "$final_output" ] || [ "$active_sha" != "$candidate_sha" ]; then
        if [ -n "$prior_path" ] && [ -n "$prior_sha" ]; then
            temporary_link="$active_link.restore.$$"
            rm -f "$temporary_link"
            if ln -s "$prior_path" "$temporary_link"; then
                mv -f "$temporary_link" "$active_link" 2>/dev/null || true
            fi
        else
            rm -f "$active_link"
        fi
        temporary_link=""
        fail active_readback_mismatch
    fi
    outcome=updated
fi

if [ -n "$prior_path" ] && [ -n "$prior_sha" ]; then
    retained_sha=$(file_sha256 "$prior_path" 2>/dev/null || true)
    if [ "$retained_sha" != "$prior_sha" ]; then
        fail prior_release_changed
    fi
fi
emit_readback "$outcome" ""
