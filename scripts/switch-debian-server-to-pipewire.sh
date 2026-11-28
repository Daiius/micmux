#!/usr/bin/env bash
# 接続先 (Debian) の音声サーバーを、PulseAudio から PipeWire (pipewire-pulse) に切り替える。
#
#   ./scripts/switch-debian-server-to-pipewire.sh --check      何も変更せず、現状と、行うことを表示する
#   ./scripts/switch-debian-server-to-pipewire.sh              切り替える
#   ./scripts/switch-debian-server-to-pipewire.sh --rollback   PulseAudio に戻す
#
# --check と切り替えには、--backports を付けられる。PipeWire を、安定版ではなく backports から入れる。
# Debian 13 の安定版の PipeWire (1.4.2) には、修正が backports (1.6.9) にしか入っていない脆弱性がある
# （prd/05-security.md §3.7）。一方、1.6 系は既定の録音デバイスの扱いが 1.4 系と違い、micmux の側の対応
# （prd/04-audio.md §1.1）が要る。実運用の接続先で確かめてあるのは、安定版のほうである。
#
# なぜ切り替えるか: PulseAudio の null sink は、録り始めてから最大で約 2 秒、音を出さない
# （prd/04-audio.md §6.1）。PipeWire では起きず、待機中の CPU の消費も無い。
#
# やること:
#   - pipewire / pipewire-pulse / wireplumber を入れる（sudo apt-get）
#   - PulseAudio のユーザーサービスを止めて、起動しないようにする（mask）
#   - PipeWire のユーザーサービスを有効にして起動する
#
# やらないこと:
#   - PulseAudio のパッケージは消さない。戻せるようにするためと、ALSA から音声サーバーへ繋ぐ
#     プラグイン（libasound2-plugins）をそのまま使うため。pipewire-pulse は PulseAudio と同じ
#     窓口を持つので、このプラグインで繋がる。
#   - pipewire-alsa は入れない。Debian では pulseaudio パッケージと同居できず、入れると
#     pulseaudio が消える。
#
# 切り替えのあいだ、音声サーバーが一度止まる。動いている micmux の接続は、SSH を開き直すと戻る。
# 何度実行しても同じ結果になる。

set -euo pipefail

PACKAGES=(pipewire pipewire-pulse wireplumber)
PULSE_UNITS=(pulseaudio.socket pulseaudio.service)
PIPEWIRE_UNITS=(pipewire.socket pipewire-pulse.socket wireplumber.service)
# 止めるときは、ソケットから起動されたサービスも含める
PIPEWIRE_ALL_UNITS=(pipewire-pulse.socket pipewire-pulse.service wireplumber.service pipewire.socket pipewire.service)
# 戻したときに、次のログインで PulseAudio と窓口を取り合わないよう、起動を止めておくもの
PIPEWIRE_MASK_UNITS=(pipewire-pulse.socket pipewire-pulse.service)
MODE=switch
APT_TARGET=()
for arg in "$@"; do
  case "$arg" in
    --backports)
      suite="$(. /etc/os-release && echo "${VERSION_CODENAME:-}")-backports"
      # いったん変数に受ける。pipefail の下で grep -q に直接つなぐと、grep が先に終わったときに失敗扱いになる
      policy="$(apt-cache policy pipewire)"
      if ! grep -q "$suite" <<<"$policy"; then
        echo "$suite is not enabled in the apt sources, so --backports cannot be used" >&2
        exit 1
      fi
      APT_TARGET=(-t "$suite")
      ;;
    --check|--rollback) MODE="$arg" ;;
    *) MODE=usage ;;
  esac
done

installed() { dpkg-query -W -f='${Status}' "$1" 2>/dev/null | grep -q 'install ok installed'; }

missing_packages() {
  local p
  for p in "${PACKAGES[@]}"; do
    installed "$p" || echo "$p"
  done
}

server_name() {
  if command -v pactl >/dev/null; then
    LC_ALL=C pactl info 2>/dev/null | sed -n 's/^Server Name: //p' || true
  fi
}

unit_state() {
  local enabled active
  enabled="$(systemctl --user is-enabled "$1" 2>/dev/null || true)"
  active="$(systemctl --user is-active "$1" 2>/dev/null || true)"
  printf '  %-26s %-10s %s\n' "$1" "${enabled:-unknown}" "${active:-unknown}"
}

show_status() {
  local p u
  echo "== sound server"
  echo "  $(server_name | grep . || echo 'cannot connect')"
  echo "== packages"
  for p in "${PACKAGES[@]}" pulseaudio libasound2-plugins pipewire-alsa; do
    if installed "$p"; then echo "  [installed] $p"; else echo "  [missing]   $p"; fi
  done
  echo "== user services (enabled / active)"
  for u in "${PULSE_UNITS[@]}" "${PIPEWIRE_ALL_UNITS[@]}"; do unit_state "$u"; done
  echo "== micmux"
  if pgrep -u "$(id -u)" -f 'micmux bridge' >/dev/null 2>&1; then
    echo "  a micmux bridge is running; it will lose its virtual mic when the sound server changes"
  else
    echo "  no micmux bridge is running"
  fi
}

confirm() {
  local answer
  read -r -p "$1 [y/N] " answer
  [ "$answer" = "y" ] || [ "$answer" = "Y" ]
}

# apt が何かを消そうとしていたら、進まない。pulseaudio が消えると、戻せなくなる
check_nothing_removed() {
  local removing
  removing="$(apt-get -s install --no-install-recommends "${APT_TARGET[@]}" -- "$@" | awk '$1 == "Remv" { print $2 }')"
  if [ -n "$removing" ]; then
    echo "apt would remove these packages, so nothing was changed:" >&2
    printf '%s\n' "$removing" | sed 's/^/  /' >&2
    exit 1
  fi
}

wait_for_server() {
  local i
  for i in $(seq 1 50); do
    if server_name | grep -q "$1"; then return 0; fi
    sleep 0.2
  done
  return 1
}

case "$MODE" in
  --check)
    show_status
    echo
    mapfile -t todo < <(missing_packages)
    if [ "${#todo[@]}" -gt 0 ]; then
      echo "would install: ${todo[*]}"
      check_nothing_removed "${todo[@]}"
      apt-get -s install --no-install-recommends "${APT_TARGET[@]}" -- "${todo[@]}" \
        | awk '$1 == "Inst" { v = ($3 ~ /^\[/) ? $4 : $3; gsub(/[()]/, "", v); print "  " $2, v }'
    else
      echo "would install: nothing (all packages are installed)"
    fi
    echo "would stop and mask: ${PULSE_UNITS[*]}"
    echo "would enable and start: ${PIPEWIRE_UNITS[*]}"
    ;;

  switch)
    show_status
    echo
    if installed pipewire-alsa; then
      echo "pipewire-alsa is installed; this script expects libasound2-plugins instead, so nothing was changed" >&2
      exit 1
    fi
    mapfile -t todo < <(missing_packages)
    if [ "${#todo[@]}" -gt 0 ]; then
      check_nothing_removed "${todo[@]}"
      echo "packages to install: ${todo[*]}"
      confirm "run sudo apt-get install?" || { echo "aborted"; exit 1; }
      sudo apt-get install -y --no-install-recommends "${APT_TARGET[@]}" -- "${todo[@]}"
    else
      echo "all packages are installed"
    fi
    if server_name | grep -q PipeWire; then
      echo "the sound server is already PipeWire"
    else
      confirm "stop PulseAudio and start PipeWire now? (voice input drops until you reopen SSH)" \
        || { echo "aborted; packages are installed but the sound server was not changed"; exit 1; }
    fi
    systemctl --user daemon-reload
    # 前に --rollback していた場合の mask を外す
    systemctl --user unmask "${PIPEWIRE_MASK_UNITS[@]}" 2>/dev/null || true
    systemctl --user disable --now "${PULSE_UNITS[@]}" 2>/dev/null || true
    # mask しておかないと、次のログインで PulseAudio も起動して、同じ窓口を取り合う
    systemctl --user mask "${PULSE_UNITS[@]}"
    systemctl --user enable --now "${PIPEWIRE_UNITS[@]}"
    if ! wait_for_server PipeWire; then
      echo "PipeWire did not come up; to go back: $0 --rollback" >&2
      show_status
      exit 1
    fi
    echo
    show_status
    echo
    echo "done; reopen your SSH connections, then run micmux doctor"
    echo "to go back to PulseAudio: $0 --rollback"
    ;;

  --rollback)
    show_status
    echo
    installed pulseaudio || { echo "pulseaudio is not installed, so there is nothing to go back to" >&2; exit 1; }
    if server_name | grep -q PipeWire || ! server_name | grep -q .; then
      confirm "stop PipeWire and start PulseAudio now? (voice input drops until you reopen SSH)" \
        || { echo "aborted"; exit 1; }
    fi
    systemctl --user daemon-reload
    systemctl --user disable --now "${PIPEWIRE_ALL_UNITS[@]}" 2>/dev/null || true
    systemctl --user mask "${PIPEWIRE_MASK_UNITS[@]}"
    systemctl --user unmask "${PULSE_UNITS[@]}"
    systemctl --user enable --now "${PULSE_UNITS[@]}"
    if ! wait_for_server pulseaudio; then
      echo "PulseAudio did not come up" >&2
      show_status
      exit 1
    fi
    echo
    show_status
    echo
    echo "done; reopen your SSH connections"
    echo "the PipeWire packages are still installed; to remove them: sudo apt-get remove ${PACKAGES[*]}"
    ;;

  *)
    printf '%s\n' \
      "Switch the sound server on the remote host (Debian) from PulseAudio to PipeWire." \
      "" \
      "  $0 --check      show the current state and what would change" \
      "  $0              switch to PipeWire" \
      "  $0 --rollback   go back to PulseAudio" \
      "" \
      "Add --backports to --check or the switch to install PipeWire from backports." >&2
    exit 2
    ;;
esac
