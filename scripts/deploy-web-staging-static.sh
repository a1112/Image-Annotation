#!/usr/bin/env bash

# Upload only the already-built web static files to the isolated staging
# service.  This script intentionally has no database, migration, or
# production-service operation in it.
set -Eeuo pipefail

readonly DEFAULT_SSH_HOST="tencent-175-178-16-90"
readonly DEFAULT_UNIT="codex-image-annotation-web-staging.service"
readonly DEFAULT_REMOTE_ROOT="/home/ubuntu/projects/.deployment/releases/image-annotation-web-staging"
readonly DEFAULT_HEALTH_URL="http://127.0.0.1:16435/health"
readonly DROPIN_NAME="90-web-dist-current.conf"

usage() {
  cat <<'EOF'
Usage:
  scripts/deploy-web-staging-static.sh [dist-web] [options]
  scripts/deploy-web-staging-static.sh --rollback REMOTE_STATIC_DIR [options]

Options:
  --host HOST              SSH host/alias (default: tencent-175-178-16-90)
  --unit UNIT              user systemd staging unit
  --remote-root DIR        retained staging static root
  --health-url URL         staging health URL as seen by the remote host
  --rollback DIR           switch to an existing retained static directory
  -h, --help               show this help

The normal form streams dist-web into a new timestamped directory, atomically
renames the complete upload, then switches WEB_DIST_DIR through a user-systemd
drop-in.  Existing static directories and drop-in history are never removed.
Production units and production-looking roots are rejected deliberately.
EOF
}

die() {
  printf 'deploy-web-staging-static: %s\n' "$*" >&2
  exit 1
}

quote_remote() {
  printf '%q' "$1"
}

ssh_host="${WEB_STAGING_SSH_HOST:-$DEFAULT_SSH_HOST}"
unit="${WEB_STAGING_SYSTEMD_UNIT:-$DEFAULT_UNIT}"
remote_root="${WEB_STAGING_REMOTE_ROOT:-$DEFAULT_REMOTE_ROOT}"
health_url="${WEB_STAGING_HEALTH_URL:-$DEFAULT_HEALTH_URL}"
dist_dir="${WEB_STAGING_DIST_DIR:-dist-web}"
rollback_target=""
positional_dist_seen="no"

while (($#)); do
  case "$1" in
    --host)
      (($# >= 2)) || die "--host requires a value"
      ssh_host="$2"
      shift 2
      ;;
    --unit)
      (($# >= 2)) || die "--unit requires a value"
      unit="$2"
      shift 2
      ;;
    --remote-root)
      (($# >= 2)) || die "--remote-root requires a value"
      remote_root="$2"
      shift 2
      ;;
    --health-url)
      (($# >= 2)) || die "--health-url requires a value"
      health_url="$2"
      shift 2
      ;;
    --rollback)
      (($# >= 2)) || die "--rollback requires a remote static directory"
      rollback_target="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      (($# == 1)) || die "only one dist-web directory is accepted"
      dist_dir="$1"
      positional_dist_seen="yes"
      shift
      ;;
    -* )
      die "unknown option: $1"
      ;;
    *)
      [[ "$positional_dist_seen" == no ]] || die "only one dist-web directory is accepted"
      dist_dir="$1"
      positional_dist_seen="yes"
      shift
      ;;
  esac
done

[[ "$unit" =~ staging ]] || die "refusing non-staging systemd unit: $unit"
[[ "$unit" != *serverall-image-annotation.service* ]] || die "refusing the production unit"
[[ "$remote_root" == /* ]] || die "remote root must be an absolute path"
[[ "$remote_root" =~ ^/[A-Za-z0-9._/-]+$ ]] || die "remote root contains unsafe characters"
[[ "$remote_root" == *staging* || "$remote_root" == *data-projects-20261026* ]] || \
  die "remote root is not recognizably isolated staging: $remote_root"
[[ "$unit" =~ ^[A-Za-z0-9_.@:-]+$ ]] || die "systemd unit contains unsafe characters"
[[ "$health_url" =~ ^https?://[A-Za-z0-9._:/-]+$ ]] || die "health URL contains unsafe characters"

if [[ -n "$rollback_target" ]]; then
  [[ "$rollback_target" == "$remote_root"/* ]] || \
    die "rollback target must be below the retained staging root"
  [[ "$rollback_target" =~ ^/[A-Za-z0-9._/-]+$ ]] || die "rollback target contains unsafe characters"
else
  [[ -d "$dist_dir" ]] || die "static directory does not exist: $dist_dir"
  [[ ! -L "$dist_dir" ]] || die "refusing a symlinked static directory: $dist_dir"
  [[ -f "$dist_dir/index.html" ]] || die "static directory has no index.html: $dist_dir"
  if [[ -n "$(find "$dist_dir" -type l -print -quit)" ]]; then
    die "static directory contains a symlink; refusing to publish it"
  fi
  if [[ -n "$(find "$dist_dir" ! -type f ! -type d -print -quit)" ]]; then
    die "static directory contains a non-file entry; refusing to publish it"
  fi
  if find "$dist_dir" -type f \( -name '._*' -o -name '.DS_Store' \) -print -quit | grep -q .; then
    die "static directory contains AppleDouble metadata"
  fi
  dist_dir="$(cd "$dist_dir" && pwd -P)"
fi

command -v ssh >/dev/null 2>&1 || die "ssh is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

remote() {
  ssh -o BatchMode=yes -o ConnectTimeout=10 "$ssh_host" "$@"
}

remote_root_q="$(quote_remote "$remote_root")"
unit_q="$(quote_remote "$unit")"

remote_home="$(remote 'printf %s "$HOME"')"
[[ "$remote_home" =~ ^/[A-Za-z0-9._/-]+$ ]] || die "remote home path is unexpected"

stamp="$(date -u +%Y%m%dT%H%M%SZ)-$$"
if [[ -n "$rollback_target" ]]; then
  target="$rollback_target"
  incoming=""
else
  target="$remote_root/$stamp"
  incoming="$remote_root/.incoming-$stamp"
fi

target_q="$(quote_remote "$target")"
# `unit` normally includes the .service suffix.  systemd's drop-in directory
# appends `.d` to the complete unit name, so avoid producing `.service.service.d`.
unit_dropin_name="$unit"
dropin_dir="$remote_home/.config/systemd/user/$unit_dropin_name.d"
history_dir="$dropin_dir/web-dist-history"
current_dropin="$dropin_dir/$DROPIN_NAME"
previous_dropin="$history_dir/$stamp.previous.conf"
missing_marker="$history_dir/$stamp.no-previous-dropin"
tmp_dropin="$dropin_dir/.${DROPIN_NAME}.${stamp}.tmp"
failed_dropin="$history_dir/$stamp.failed-new.conf"
current_overlay="$dropin_dir/web-dist-current.env"
previous_overlay="$history_dir/$stamp.previous.env"
missing_overlay_marker="$history_dir/$stamp.no-previous-env"
tmp_overlay="$dropin_dir/.web-dist-current.${stamp}.tmp"
failed_overlay="$history_dir/$stamp.failed-new.env"

dropin_dir_q="$(quote_remote "$dropin_dir")"
history_dir_q="$(quote_remote "$history_dir")"
current_dropin_q="$(quote_remote "$current_dropin")"
previous_dropin_q="$(quote_remote "$previous_dropin")"
missing_marker_q="$(quote_remote "$missing_marker")"
tmp_dropin_q="$(quote_remote "$tmp_dropin")"
failed_dropin_q="$(quote_remote "$failed_dropin")"
current_overlay_q="$(quote_remote "$current_overlay")"
previous_overlay_q="$(quote_remote "$previous_overlay")"
missing_overlay_marker_q="$(quote_remote "$missing_overlay_marker")"
tmp_overlay_q="$(quote_remote "$tmp_overlay")"
failed_overlay_q="$(quote_remote "$failed_overlay")"
health_url_q="$(quote_remote "$health_url")"
target_env_q="$(quote_remote "WEB_DIST_DIR=$target")"

if [[ -n "$rollback_target" ]]; then
  printf 'checking retained staging static directory: %s\n' "$target"
  remote "set -eu; test -d $target_q; test -f $target_q/index.html"
else
  printf 'creating retained staging static directory: %s\n' "$target"
  remote "set -eu; mkdir -p $remote_root_q; test ! -e $(quote_remote "$incoming"); mkdir $incoming"
  # Tar is streamed over SSH so the upload is bounded by the remote staging
  # directory and never collected into a shell variable or memory buffer.
  if ! COPYFILE_DISABLE=1 tar -cf - --no-xattrs -C "$dist_dir" . | remote "set -eu; tar -C $(quote_remote "$incoming") -xpf -"; then
    die "static upload failed; incomplete upload was retained at $incoming"
  fi
  remote "set -eu; test -f $(quote_remote "$incoming/index.html"); chmod -R a+rX $(quote_remote "$incoming")"
fi

previous_state=""
if ! previous_state="$(remote "set -eu; mkdir -p $dropin_dir_q $history_dir_q; if test -L $current_dropin_q; then exit 42; elif test -f $current_dropin_q; then cp -p $current_dropin_q $previous_dropin_q; printf '%s' yes; else : > $missing_marker_q; printf '%s' no; fi")"; then
  die "refusing to overwrite an invalid or unreadable existing drop-in"
fi
case "$previous_state" in
  yes) previous_exists="yes" ;;
  no) previous_exists="no" ;;
  *) die "unexpected drop-in state returned by remote host" ;;
esac

previous_overlay_state=""
if ! previous_overlay_state="$(remote "set -eu; if test -L $current_overlay_q; then exit 42; elif test -f $current_overlay_q; then cp -p $current_overlay_q $previous_overlay_q; printf '%s' yes; else : > $missing_overlay_marker_q; printf '%s' no; fi")"; then
  die "refusing to overwrite an invalid or unreadable existing WEB_DIST_DIR overlay"
fi
case "$previous_overlay_state" in
  yes) previous_overlay_exists="yes" ;;
  no) previous_overlay_exists="no" ;;
  *) die "unexpected WEB_DIST_DIR overlay state returned by remote host" ;;
esac

# Keep old hashed chunks available for tabs that still hold the previous
# index.html.  The new index is deliberately the only file that may replace
# an old file; every other same-name collision must be byte-identical.
previous_static_dir=""
if [[ "$previous_overlay_exists" == yes ]]; then
  previous_static_dir="$(remote "set -eu; value=\$(sed -n 's/^WEB_DIST_DIR=//p' $current_overlay_q); test \"\$value\" != \"\"; printf '%s' \"\$value\"")" || \
    die "existing WEB_DIST_DIR overlay is malformed"
  [[ "$previous_static_dir" =~ ^/[A-Za-z0-9._/-]+$ ]] || \
    die "existing WEB_DIST_DIR is unsafe: $previous_static_dir"
  previous_static_q="$(quote_remote "$previous_static_dir")"
  remote "set -eu; test -d $previous_static_q; test ! -L $previous_static_q"
  remote "set -eu
    previous=$previous_static_q
    incoming=$(quote_remote "$incoming")
    test -z \"\$(find \"\$previous\" -type l -print -quit)\"
    test -z \"\$(find \"\$previous\" ! -type f ! -type d -print -quit)\"
    while IFS= read -r -d '' source; do
      rel=\"\${source#\"\$previous\"/}\"
      case \"\$rel\" in
        index.html|._*|*/._*|.DS_Store|*/.DS_Store) continue ;;
      esac
      destination=\"\$incoming/\$rel\"
      if test -e \"\$destination\"; then
        test ! -L \"\$destination\"
        cmp -s \"\$source\" \"\$destination\" || {
          printf 'same-name static asset differs: %s\\n' \"\$rel\" >&2
          exit 1
        }
      else
        mkdir -p \"\$(dirname \"\$destination\")\"
        cp -p \"\$source\" \"\$destination\"
      fi
    done < <(find \"\$previous\" -type f -print0)
  " || die "previous staging assets could not be retained safely"
else
  printf 'no prior WEB_DIST_DIR overlay; skipping old static asset retention\n'
fi

remote "set -eu; test ! -e $target_q; mv $(quote_remote "$incoming") $target_q"

restore_dropin() {
  if [[ "$previous_exists" == yes ]]; then
    remote "set -eu; cp -p $previous_dropin_q $tmp_dropin_q; mv $tmp_dropin_q $current_dropin_q" || return 1
  else
    # Keep the attempted new configuration as evidence instead of deleting it.
    remote "set -eu; if test -e $current_dropin_q; then mv $current_dropin_q $failed_dropin_q; fi" || return 1
  fi
  if [[ "$previous_overlay_exists" == yes ]]; then
    remote "set -eu; cp -p $previous_overlay_q $tmp_overlay_q; mv $tmp_overlay_q $current_overlay_q" || return 1
  else
    remote "set -eu; if test -e $current_overlay_q; then mv $current_overlay_q $failed_overlay_q; fi" || return 1
  fi
  remote "set -eu; systemctl --user daemon-reload; systemctl --user restart $unit_q" || return 1
}

printf 'switching staging WEB_DIST_DIR through user-systemd drop-in\n'
if ! remote "set -eu; printf '%s\\n' 'WEB_DIST_DIR=$target' > $tmp_overlay_q; chmod 600 $tmp_overlay_q; mv $tmp_overlay_q $current_overlay_q; printf '%s\\n' '[Service]' 'EnvironmentFile=$current_overlay' > $tmp_dropin_q; chmod 600 $tmp_dropin_q; mv $tmp_dropin_q $current_dropin_q; systemctl --user daemon-reload; systemctl --user restart $unit_q"; then
  restore_dropin || die "switch failed and automatic drop-in restore also failed; inspect $history_dir"
  die "staging restart failed; previous static directory was restored"
fi

applied_environment="$(remote "systemctl --user show $unit_q -p Environment --value")"
environment_verified="no"
if [[ "$applied_environment" == *"WEB_DIST_DIR=$target"* ]] || remote "set -eu; pid=\$(systemctl --user show $unit_q -p MainPID --value); test \"\$pid\" -gt 0; grep -Fzx $target_env_q /proc/\$pid/environ >/dev/null"; then
  environment_verified="yes"
fi
if [[ "$environment_verified" != yes ]]; then
  restore_dropin || die "WEB_DIST_DIR was not applied and automatic restore also failed; inspect $history_dir"
  die "systemd did not apply the requested WEB_DIST_DIR; previous static directory was restored"
fi

if ! remote "curl -fsS --max-time 10 $health_url_q >/dev/null"; then
  restore_dropin || die "health check failed and automatic restore also failed; inspect $history_dir"
  die "staging health check failed; previous static directory was restored"
fi

printf 'staging static update is healthy: %s\n' "$target"
printf 'drop-in history: %s\n' "$history_dir"
printf 'rollback command: %s --rollback %s\n' "$0" "$target"
