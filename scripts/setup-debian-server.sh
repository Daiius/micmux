#!/usr/bin/env bash
# 接続先 (Debian) に、micmux bridge が必要とする音声基盤を入れる。
#
#   ./scripts/setup-debian-server.sh --check    何も変更せず、現状だけ表示する
#   ./scripts/setup-debian-server.sh            不足しているパッケージを入れ、ユーザーサービスを起動する
#   ./scripts/setup-debian-server.sh --remove   このスクリプトが入れたパッケージを取り除く
#
# 入れるもの:
#   pulseaudio          ユーザー権限で動くサウンドサーバー（仮想マイクの置き場所）
#   pulseaudio-utils    pactl / pacat（bridge が使う）
#   libasound2-plugins  ALSA → PulseAudio の橋渡し（Claude Code は ALSA で録音する）
#
# 既に PipeWire (pipewire-pulse) が動いている環境では PulseAudio を入れない。
# 何度実行しても同じ結果になる。

set -euo pipefail

PACKAGES=(pulseaudio pulseaudio-utils libasound2-plugins)
STATE_FILE="${XDG_STATE_HOME:-$HOME/.local/state}/micmux/installed-packages"
MODE="${1:-install}"

installed() { dpkg-query -W -f='${Status}' "$1" 2>/dev/null | grep -q 'install ok installed'; }

missing_packages() {
  local p
  for p in "${PACKAGES[@]}"; do
    installed "$p" || echo "$p"
  done
}

show_status() {
  local p
  echo "== packages"
  for p in "${PACKAGES[@]}" pipewire-pulse alsa-utils; do
    if installed "$p"; then echo "  [installed] $p"; else echo "  [missing]   $p"; fi
  done
  echo "== sound server"
  if command -v pactl >/dev/null && pactl info >/dev/null 2>&1; then
    LC_ALL=C pactl info | grep -E '^(Server Name|Default Source):' | sed 's/^/  /'
  else
    echo "  cannot connect"
  fi
  echo "== keep user services running after logout (linger)"
  echo "  $(loginctl show-user "$USER" 2>/dev/null | grep -i '^Linger=' || echo 'Linger=unknown')"
  echo "== sound cards (/proc/asound/cards)"
  sed 's/^/  /' /proc/asound/cards 2>/dev/null || echo "  cannot read"
  if [ -f "$STATE_FILE" ]; then
    echo "== packages installed by this script"
    sed 's/^/  /' "$STATE_FILE"
  fi
}

confirm() {
  local answer
  read -r -p "$1 [y/N] " answer
  [ "$answer" = "y" ] || [ "$answer" = "Y" ]
}

case "$MODE" in
  --check)
    show_status
    ;;

  install)
    show_status
    echo
    if command -v pactl >/dev/null && LC_ALL=C pactl info 2>/dev/null | grep -q 'PipeWire'; then
      echo "PipeWire is running, so PulseAudio will not be installed"
      echo "make sure pipewire-alsa is installed for apps that record via ALSA"
      exit 0
    fi
    mapfile -t todo < <(missing_packages)
    if [ "${#todo[@]}" -gt 0 ]; then
      echo "packages to install: ${todo[*]}"
      confirm "run sudo apt-get install?" || { echo "aborted"; exit 1; }
      sudo apt-get install -y --no-install-recommends -- "${todo[@]}"
      mkdir -p "$(dirname "$STATE_FILE")"
      printf '%s\n' "${todo[@]}" >> "$STATE_FILE"
      sort -u -o "$STATE_FILE" "$STATE_FILE"
    else
      echo "all required packages are installed"
    fi
    # ソケット起動にしておけば、最初に使われたときだけサーバーが立ち上がる
    systemctl --user daemon-reload
    systemctl --user enable --now pulseaudio.socket
    echo
    show_status
    echo
    echo "done; run micmux doctor to check the setup"
    echo "to undo: $0 --remove"
    ;;

  --remove)
    if [ ! -s "$STATE_FILE" ]; then
      echo "no record of packages installed by this script ($STATE_FILE)"
      exit 0
    fi
    # 記録の各行は sudo apt-get の引数になる。書き換えられていても、このスクリプトが入れうる
    # パッケージ名以外（たとえば -o で始まる apt のオプション）は通さない
    todo=()
    while IFS= read -r line; do
      for p in "${PACKAGES[@]}"; do
        if [ "$line" = "$p" ]; then todo+=("$p"); fi
      done
    done < "$STATE_FILE"
    if [ "${#todo[@]}" -eq 0 ]; then
      echo "the record ($STATE_FILE) lists no packages this script installs"
      exit 0
    fi
    echo "packages installed by this script: ${todo[*]}"
    # apt-get remove はこれらに依存するパッケージも一緒に消す。記録だけを見せて確認すると、
    # 利用者の知らないものまで消えるので、実際に消えるものを模擬実行で調べて見せる
    removing="$(apt-get -s remove -- "${todo[@]}" | awk '$1 == "Remv" { print $2 }')"
    if [ -z "$removing" ]; then
      echo "nothing to remove (already uninstalled)"
      rm -f "$STATE_FILE"
      exit 0
    fi
    echo "packages that will actually be removed (including dependents):"
    printf '%s\n' "$removing" | sed 's/^/  /'
    confirm "run sudo apt-get remove?" || { echo "aborted"; exit 1; }
    systemctl --user disable --now pulseaudio.socket pulseaudio.service 2>/dev/null || true
    sudo apt-get remove -y -- "${todo[@]}"
    rm -f "$STATE_FILE"
    echo "removed; libraries pulled in as dependencies can be removed with sudo apt-get autoremove"
    ;;

  *)
    printf '%s\n' \
      "Install the audio stack that micmux bridge needs on the remote host (Debian)." \
      "" \
      "  $0 --check    show the current state without changing anything" \
      "  $0            install missing packages and start the user service" \
      "  $0 --remove   remove the packages this script installed"
    exit 2
    ;;
esac
