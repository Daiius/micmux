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
  echo "== パッケージ"
  for p in "${PACKAGES[@]}" pipewire-pulse alsa-utils; do
    if installed "$p"; then echo "  [入っている] $p"; else echo "  [無い]       $p"; fi
  done
  echo "== サウンドサーバー"
  if command -v pactl >/dev/null && pactl info >/dev/null 2>&1; then
    LC_ALL=C pactl info | grep -E '^(Server Name|Default Source):' | sed 's/^/  /'
  else
    echo "  接続できません"
  fi
  echo "== ログアウト後もユーザーサービスを動かす設定 (linger)"
  echo "  $(loginctl show-user "$USER" 2>/dev/null | grep -i '^Linger=' || echo 'Linger=不明')"
  echo "== サウンドカード (/proc/asound/cards)"
  sed 's/^/  /' /proc/asound/cards 2>/dev/null || echo "  読めません"
  if [ -f "$STATE_FILE" ]; then
    echo "== このスクリプトが入れたパッケージ"
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
      echo "PipeWire が動いています。PulseAudio は入れません。"
      echo "ALSA で録音するアプリのために pipewire-alsa が入っているかだけ確認してください。"
      exit 0
    fi
    mapfile -t todo < <(missing_packages)
    if [ "${#todo[@]}" -gt 0 ]; then
      echo "これから入れるパッケージ: ${todo[*]}"
      confirm "sudo apt-get install を実行しますか?" || { echo "中止しました"; exit 1; }
      sudo apt-get install -y --no-install-recommends "${todo[@]}"
      mkdir -p "$(dirname "$STATE_FILE")"
      printf '%s\n' "${todo[@]}" >> "$STATE_FILE"
      sort -u -o "$STATE_FILE" "$STATE_FILE"
    else
      echo "必要なパッケージは揃っています"
    fi
    # ソケット起動にしておけば、最初に使われたときだけサーバーが立ち上がる
    systemctl --user daemon-reload
    systemctl --user enable --now pulseaudio.socket
    echo
    show_status
    echo
    echo "完了。micmux doctor で点検できます。"
    echo "元に戻すには: $0 --remove"
    ;;

  --remove)
    if [ ! -s "$STATE_FILE" ]; then
      echo "このスクリプトが入れたパッケージの記録がありません ($STATE_FILE)"
      exit 0
    fi
    mapfile -t todo < "$STATE_FILE"
    echo "このスクリプトが入れたパッケージ: ${todo[*]}"
    # apt-get remove はこれらに依存するパッケージも一緒に消す。記録だけを見せて確認すると、
    # 利用者の知らないものまで消えるので、実際に消えるものを模擬実行で調べて見せる
    removing="$(apt-get -s remove "${todo[@]}" | awk '$1 == "Remv" { print $2 }')"
    if [ -z "$removing" ]; then
      echo "取り除くものはありません（既に入っていない）"
      rm -f "$STATE_FILE"
      exit 0
    fi
    echo "実際に取り除かれるパッケージ（依存しているものを含む）:"
    printf '%s\n' "$removing" | sed 's/^/  /'
    confirm "sudo apt-get remove を実行しますか?" || { echo "中止しました"; exit 1; }
    systemctl --user disable --now pulseaudio.socket pulseaudio.service 2>/dev/null || true
    sudo apt-get remove -y "${todo[@]}"
    rm -f "$STATE_FILE"
    echo "取り除きました。依存で入ったライブラリは sudo apt-get autoremove で消せます。"
    ;;

  *)
    sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
