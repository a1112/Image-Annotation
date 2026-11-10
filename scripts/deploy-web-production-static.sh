#!/usr/bin/env bash

# Frontend-only production static switch. This script never reads database
# credentials, copies a server binary, or edits ExecStart/WorkingDirectory.
set -Eeuo pipefail

readonly DEFAULT_HOST="tencent-175-178-16-90"
readonly SERVICE="serverall-image-annotation.service"
readonly STATIC_ROOT="/home/ubuntu/projects/.deployment/releases/image-annotation-web-static"
readonly DROPIN_NAME="98-codex-web-static-density.conf"
readonly HEALTH_URL="http://127.0.0.1:16423/health"

usage() {
  cat <<'EOF'
Usage:
  scripts/deploy-web-production-static.sh --dist-web DIR --confirm-production [options]
  scripts/deploy-web-production-static.sh --prepare --dist-web DIR --confirm-production [options]
  scripts/deploy-web-production-static.sh --candidate REMOTE_DIR --confirm-production [options]
  scripts/deploy-web-production-static.sh --rollback --confirm-production [options]

Options:
  --prepare                 Upload/merge a candidate only; never write 98.
  --candidate REMOTE_DIR   Apply an already prepared retained candidate.
  --host HOST               SSH host (default: tencent-175-178-16-90).
  --health-url URL          Production health URL as seen by the remote host.
  --confirm-production      Required for every mutation.
  -h, --help                Show this help.

Apply writes only a 98 Environment=WEB_DIST_DIR drop-in and keeps 95/96/97
unchanged. Rollback moves 98 to private history so unchanged 97 is selected.
Previous hashed assets are retained except index.html and old AppleDouble files.
EOF
}

die() {
  printf 'deploy-web-production-static: %s\n' "$*" >&2
  exit 1
}

quote_remote() {
  printf '%q' "$1"
}

host="${PRODUCTION_SSH_HOST:-$DEFAULT_HOST}"
health_url="$HEALTH_URL"
dist_dir=""
candidate=""
mode=""
confirmed="no"

while (($#)); do
  case "$1" in
    --dist-web)
      (($# >= 2)) || die "--dist-web requires a directory"
      dist_dir="$2"
      [[ -z "$mode" || "$mode" == prepare ]] || die "choose one static operation"
      [[ "$mode" == prepare ]] || mode="apply"
      shift 2
      ;;
    --prepare)
      [[ -z "$mode" || "$mode" == apply ]] || die "choose one static operation"
      mode="prepare"
      shift
      ;;
    --candidate)
      (($# >= 2)) || die "--candidate requires a retained remote directory"
      [[ -z "$mode" ]] || die "choose one static operation"
      candidate="$2"
      mode="candidate"
      shift 2
      ;;
    --rollback)
      [[ -z "$mode" ]] || die "choose either --dist-web or --rollback"
      mode="rollback"
      shift
      ;;
    --host)
      (($# >= 2)) || die "--host requires a value"
      host="$2"
      shift 2
      ;;
    --health-url)
      (($# >= 2)) || die "--health-url requires a value"
      health_url="$2"
      shift 2
      ;;
    --confirm-production)
      confirmed="yes"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

[[ "$confirmed" == yes ]] || die "--confirm-production is required"
[[ -n "$mode" ]] || die "choose --dist-web or --rollback"
[[ "$host" =~ ^[A-Za-z0-9._:-]+$ ]] || die "unsafe SSH host"
[[ "$health_url" =~ ^https?://[A-Za-z0-9._:/-]+$ ]] || die "unsafe health URL"
command -v ssh >/dev/null 2>&1 || die "ssh is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

if [[ "$mode" == apply || "$mode" == prepare ]]; then
  [[ -d "$dist_dir" && ! -L "$dist_dir" ]] || die "invalid dist-web directory: $dist_dir"
  [[ -f "$dist_dir/index.html" ]] || die "dist-web has no index.html"
  [[ -z "$(find "$dist_dir" -type l -print -quit)" ]] || die "dist-web contains a symlink"
  [[ -z "$(find "$dist_dir" ! -type f ! -type d -print -quit)" ]] || die "dist-web contains a non-file entry"
  if find "$dist_dir" -type f \( -name '._*' -o -name '.DS_Store' \) -print -quit | grep -q .; then
    die "dist-web contains AppleDouble metadata"
  fi
  dist_dir="$(cd "$dist_dir" && pwd -P)"
fi
if [[ "$mode" == candidate ]]; then
  [[ "$candidate" =~ ^/[A-Za-z0-9._/-]+$ ]] || die "unsafe candidate path"
fi

remote() {
  ssh -o BatchMode=yes -o ConnectTimeout=10 "$host" "$@"
}

[[ "$STATIC_ROOT" =~ ^/[A-Za-z0-9._/-]+$ ]] || die "unsafe static root"
unit_q="$(quote_remote "$SERVICE")"
static_root_q="$(quote_remote "$STATIC_ROOT")"
health_url_q="$(quote_remote "$health_url")"
remote_home="$(remote 'printf %s "$HOME"')"
[[ "$remote_home" =~ ^/[A-Za-z0-9._/-]+$ ]] || die "unexpected remote home"
dropin_dir="/etc/systemd/system/$SERVICE.d"
history="$dropin_dir/.codex-web-static-density-history"
dropin="$dropin_dir/$DROPIN_NAME"
stamp="$(date -u +%Y%m%dT%H%M%SZ)-$$"
dropin_dir_q="$(quote_remote "$dropin_dir")"
history_q="$(quote_remote "$history")"
dropin_q="$(quote_remote "$dropin")"

read_process_state() {
  local output
  output="$(remote "set -eu; pid=\$(sudo -n systemctl show $unit_q -p MainPID --value); test \"\$pid\" -gt 0; exe=\$(readlink -f /proc/\$pid/exe); web=\$(tr '\\0' '\\n' < /proc/\$pid/environ | sed -n 's/^WEB_DIST_DIR=//p' | head -n 1); test \"\$exe\" != \"\"; test \"\$web\" != \"\"; printf '%s\\n' \"\$pid\" \"\$exe\" \"\$web\"")" || die "could not read active production process state"
  PROCESS_STATE=()
  while IFS= read -r line; do
    PROCESS_STATE[${#PROCESS_STATE[@]}]="$line"
  done <<<"$output"
  [[ "${#PROCESS_STATE[@]}" -eq 3 ]] || die "unexpected production process state"
  [[ "${PROCESS_STATE[0]}" =~ ^[0-9]+$ ]] || die "unexpected production PID"
  [[ "${PROCESS_STATE[1]}" =~ ^/[A-Za-z0-9._/-]+$ ]] || die "unexpected production binary path"
  [[ "${PROCESS_STATE[2]}" =~ ^/[A-Za-z0-9._/-]+$ ]] || die "unexpected production WEB_DIST_DIR"
}

wait_for_health() {
  remote "set -eu; i=0; while test \"\$i\" -lt 45; do if curl -fsS --max-time 2 $health_url_q >/dev/null 2>&1; then exit 0; fi; i=\$((i + 1)); sleep 1; done; exit 1"
}

read_process_state
before_pid="${PROCESS_STATE[0]}"
before_exe="${PROCESS_STATE[1]}"
before_web="${PROCESS_STATE[2]}"

if [[ "$mode" == apply || "$mode" == prepare || "$mode" == candidate ]]; then
  if [[ "$mode" == candidate ]]; then
    target="$candidate"
    target_q="$(quote_remote "$target")"
    remote "set -eu; test -d $target_q; test -f $target_q/index.html; test -z \"\$(find $target_q -type f \\( -name '._*' -o -name '.DS_Store' \\) -print -quit)\""
  else
    target="$STATIC_ROOT/$stamp"
    incoming="$STATIC_ROOT/.incoming-$stamp"
    target_q="$(quote_remote "$target")"
    incoming_q="$(quote_remote "$incoming")"
  fi
  previous_web_q="$(quote_remote "$before_web")"

  if [[ "$mode" != candidate ]]; then
    remote "set -eu; mkdir -p $static_root_q; test ! -e $incoming_q; mkdir $incoming_q"
    if ! COPYFILE_DISABLE=1 tar -cf - --no-xattrs --exclude='./.DS_Store' --exclude='./._*' --exclude='*/.DS_Store' --exclude='*/._*' -C "$dist_dir" . | remote "set -eu; tar -C $incoming_q -xpf -"; then
      die "static upload failed; incomplete incoming directory retained at $incoming"
    fi
    remote "set -eu; test -f $incoming_q/index.html; test -z \"\$(find $incoming_q -type f \\( -name '._*' -o -name '.DS_Store' \\) -print -quit)\""
    remote "set -eu; test -d $previous_web_q; test ! -L $previous_web_q; test -z \"\$(find $previous_web_q -type l -print -quit)\"; test -z \"\$(find $previous_web_q ! -type f ! -type d -print -quit)\"; while IFS= read -r -d '' source; do rel=\"\${source#$previous_web_q/}\"; case \"\$rel\" in index.html|._*|*/._*|.DS_Store|*/.DS_Store) continue ;; esac; destination=\"$incoming_q/\$rel\"; if test -e \"\$destination\"; then test ! -L \"\$destination\"; cmp -s \"\$source\" \"\$destination\" || { printf 'same-name static asset differs: %s\\n' \"\$rel\" >&2; exit 1; }; else mkdir -p \"\$(dirname \"\$destination\")\"; cp -p \"\$source\" \"\$destination\"; fi; done < <(find $previous_web_q -type f -print0); test -z \"\$(find $incoming_q -type f \\( -name '._*' -o -name '.DS_Store' \\) -print -quit)\"; chmod -R a+rX $incoming_q; test ! -e $target_q; mv $incoming_q $target_q"
  fi

  if [[ "$mode" == prepare ]]; then
    printf 'production_static_candidate=%s\n' "$target"
    printf 'production_static_switch_executed=false\n'
    exit 0
  fi

  previous_98_state="$(remote "set -eu; sudo -n mkdir -p $history_q; for f in 95-codex-web-accepted-20261027.conf 96-codex-web-static.conf 97-codex-web-data-project-groups.conf; do src=$dropin_dir/\$f; if sudo -n test -L \"\$src\"; then exit 42; elif sudo -n test -f \"\$src\"; then sudo -n cp \"\$src\" $history_q/$stamp.previous-\$f; fi; done; if sudo -n test -L $dropin_q; then exit 43; elif sudo -n test -f $dropin_q; then sudo -n cp $dropin_q $history_q/$stamp.previous-98.conf; printf yes; else printf no; fi; printf '%s\\n' 'pid=$before_pid' 'exe=$before_exe' 'webDistDir=$before_web' | sudo -n tee $history_q/$stamp.previous-reference.txt >/dev/null; sudo -n chmod 600 $history_q/$stamp.previous-reference.txt")" || die "existing production drop-ins are unsafe or unreadable"
  case "$previous_98_state" in
    yes) previous_98_exists=yes ;;
    no) previous_98_exists=no ;;
    *) die "unexpected 98 drop-in state" ;;
  esac

  restore_dropin_and_restart() {
    if [[ "$previous_98_exists" == yes ]]; then
      remote "set -eu; sudo -n cp $(quote_remote "$history/$stamp.previous-98.conf") $dropin_q; sudo -n chmod 600 $dropin_q; sudo -n systemctl daemon-reload; sudo -n systemctl reset-failed $unit_q; sudo -n systemctl restart $unit_q" || return 1
    else
      remote "set -eu; sudo -n rm -f $dropin_q; sudo -n systemctl daemon-reload; sudo -n systemctl reset-failed $unit_q; sudo -n systemctl restart $unit_q" || return 1
    fi
    wait_for_health
  }

  tmp_dropin="$dropin_dir/.${DROPIN_NAME}.${stamp}.tmp"
  tmp_dropin_q="$(quote_remote "$tmp_dropin")"
  remote "set -eu; printf '%s\\n' '[Service]' 'Environment=WEB_DIST_DIR=$target' | sudo -n tee $tmp_dropin_q >/dev/null; sudo -n chmod 600 $tmp_dropin_q; sudo -n mv $tmp_dropin_q $dropin_q; sudo -n systemctl daemon-reload"
  if ! remote "set -eu; sudo -n systemctl reset-failed $unit_q; sudo -n systemctl restart $unit_q" || ! wait_for_health; then
    restore_dropin_and_restart || die "static switch failed and 98 restoration failed; inspect $history"
    die "static switch health failed; previous 98 was restored"
  fi
  read_process_state
  if [[ "$before_exe" != "${PROCESS_STATE[1]}" || "$target" != "${PROCESS_STATE[2]}" ]]; then
    restore_dropin_and_restart || die "static switch changed protected process state and restoration failed"
    die "static switch changed binary or WEB_DIST_DIR unexpectedly; previous 98 was restored"
  fi
  remote "set -eu; printf '%s\\n' '{' '  \"mode\": \"apply\",' '  \"target\": \"$target\",' '  \"binaryChanged\": false,' '  \"databaseChanged\": false,' '  \"health\": \"ok\",' '  \"rollback\": \"$0 --rollback --confirm-production\"' '}' | sudo -n tee $history_q/$stamp.apply-receipt.json >/dev/null; sudo -n chmod 600 $history_q/$stamp.apply-receipt.json"
  printf 'production_static_release=%s\n' "$target"
  printf 'production_static_rollback=%s --rollback --confirm-production\n' "$0"
  exit 0
fi

# Rollback only removes the 98 override. The 97 binary/drop-in and static
# release remain untouched.
remote "set -eu; sudo -n mkdir -p $history_q; if sudo -n test -L $dropin_q; then exit 42; elif sudo -n test -f $dropin_q; then sudo -n cp $dropin_q $history_q/$stamp.rollback-98.conf; else exit 44; fi; sudo -n test -f $dropin_dir_q/97-codex-web-data-project-groups.conf; base=\$(sudo -n sed -n 's/^Environment=WEB_DIST_DIR=//p' $dropin_dir_q/97-codex-web-data-project-groups.conf | tail -n 1); test \"\$base\" != \"\"; printf '%s\\n' \"\$base\" | sudo -n tee $history_q/$stamp.rollback-reference.txt >/dev/null; sudo -n chmod 600 $history_q/$stamp.rollback-reference.txt"
rollback_98="$history/$stamp.rollback-98.conf"
rollback_98_q="$(quote_remote "$rollback_98")"
remote "set -eu; sudo -n mv $dropin_q $history_q/$stamp.rollback-moved-98.conf; sudo -n systemctl daemon-reload; sudo -n systemctl reset-failed $unit_q; sudo -n systemctl restart $unit_q" || {
  remote "set -eu; sudo -n cp $rollback_98_q $dropin_q; sudo -n chmod 600 $dropin_q; sudo -n systemctl daemon-reload; sudo -n systemctl reset-failed $unit_q; sudo -n systemctl restart $unit_q" || die "rollback failed and 98 restoration failed"
  wait_for_health || die "rollback failed and restored 98 did not become healthy"
  die "rollback restart failed; 98 was restored"
}
if ! wait_for_health; then
  remote "set -eu; sudo -n cp $rollback_98_q $dropin_q; sudo -n chmod 600 $dropin_q; sudo -n systemctl daemon-reload; sudo -n systemctl reset-failed $unit_q; sudo -n systemctl restart $unit_q" || die "rollback health failed and 98 restoration failed"
  wait_for_health || die "rollback health failed and restored 98 did not become healthy"
  die "rollback health failed; 98 was restored"
fi
read_process_state
if [[ "$before_exe" != "${PROCESS_STATE[1]}" ]]; then
  remote "set -eu; sudo -n cp $rollback_98_q $dropin_q; sudo -n chmod 600 $dropin_q; sudo -n systemctl daemon-reload; sudo -n systemctl reset-failed $unit_q; sudo -n systemctl restart $unit_q" || die "rollback changed binary and 98 restoration failed"
  wait_for_health || die "rollback changed binary and restored 98 did not become healthy"
  die "rollback changed the protected binary; 98 was restored"
fi
remote "set -eu; printf '%s\\n' '{' '  \"mode\": \"rollback\",' '  \"binaryChanged\": false,' '  \"databaseChanged\": false,' '  \"health\": \"ok\",' '  \"meaning\": \"98 removed; unchanged 97 remains selected\"' '}' | sudo -n tee $history_q/$stamp.rollback-receipt.json >/dev/null; sudo -n chmod 600 $history_q/$stamp.rollback-receipt.json"
printf 'production_static_rollback=ok\n'
printf 'protected_97_unchanged=yes\n'
