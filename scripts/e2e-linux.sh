#!/usr/bin/env bash
# Linux 上で、client → bridge → 仮想マイクの流れを通しで確かめる。
#
#   ./scripts/e2e-linux.sh <micmux のバイナリ>
#   E2E_NATIVE=1 ./scripts/e2e-linux.sh <--features native でビルドしたバイナリ>
#
# 代役のマイク（音を流し込んだ null sink）から client が取得し、bridge を通って仮想マイクに
# 届いた音を録って照合する。実際のマイクも SSH も使わない（--transport で bridge を直接起動する）。
#
# 利用者の音声サーバーには触れない。専用の PulseAudio を一時ディレクトリに立て、起動したものは
# すべて PID を指定して止める。手元でも CI でも同じように動く。
#
# 要るもの: pulseaudio, pactl, pacat, parec, python3。
# E2E_NATIVE=1 のときは、ALSA から PulseAudio へ繋ぐプラグイン（libasound2-plugins）も要る。

set -euo pipefail

BIN="${1:?使い方: $0 <micmux のバイナリ>}"
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
[ -x "$BIN" ] || { echo "実行できません: $BIN" >&2; exit 2; }
for c in pulseaudio pactl pacat parec python3; do
  command -v "$c" >/dev/null || { echo "$c がありません" >&2; exit 2; }
done

W="$(mktemp -d)"
PIDS=()
FAILED=0

# ここから先は、すべて専用の音声サーバーと専用のソケットに向く
export PULSE_RUNTIME_PATH="$W/pulse-rt" PULSE_STATE_PATH="$W/pulse-state" PULSE_SERVER="unix:$W/pulse-rt/native"
export MICMUX_SOCKET="$W/micmux.sock" XDG_STATE_HOME="$W/state" XDG_RUNTIME_DIR="$W/rt"
unset PULSE_SINK PULSE_SOURCE
mkdir -p "$W/pulse-rt" "$W/rt"

cleanup() {
  local p
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
  if [ "$FAILED" -ne 0 ]; then
    echo "---- client.log"; cat "$W/client.log" 2>/dev/null || true
    echo "---- pulse.log";  tail -n 20 "$W/pulse.log" 2>/dev/null || true
  fi
  rm -rf "$W"
}
trap cleanup EXIT

fail() { echo "NG: $*" >&2; FAILED=1; exit 1; }
ok()   { echo "ok: $*"; }

# 条件が成り立つまで待つ（0.1 秒刻み）。第 1 引数は上限の秒数
wait_for() {
  local limit=$(( $1 * 10 )) i; shift
  for (( i = 0; i < limit; i++ )); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.1
  done
  return 1
}

# 録った音（s16le・16 kHz・モノラル）の後半を調べ、「rms 周波数」を出す
analyze() {
  python3 - "$1" <<'PY'
import struct, sys, math
d = open(sys.argv[1], 'rb').read()
n = len(d) // 2
s = struct.unpack('<%dh' % n, d[:n * 2])[n // 2:]
if len(s) < 1600:
    print("0 0"); sys.exit()
rms = math.sqrt(sum(x * x for x in s) / len(s))
crossings = sum(1 for i in range(1, len(s)) if (s[i - 1] < 0) != (s[i] < 0))
print(round(rms), round(crossings / 2 / (len(s) / 16000)))
PY
}

status_has() { "$BIN" status 2>/dev/null | grep -q "$1"; }
sink_exists() { pactl list short sinks | cut -f2 | grep -qx "$1"; }

echo "== 専用の音声サーバーを立てる"
# D-Bus を切り離さないと、利用者の音声サーバーと名前が衝突して起動しない
DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent pulseaudio -n \
  -L "module-native-protocol-unix socket=$W/pulse-rt/native" \
  --daemonize=no --exit-idle-time=-1 --use-pid-file=no --log-target="file:$W/pulse.log" &
PIDS+=($!)
wait_for 10 pactl info || fail "音声サーバーが起動しない"
pactl info | grep -q "$W" || fail "専用の音声サーバーに繋がっていない"
ok "音声サーバー"

echo "== 代役のマイクに 1 kHz の音を流し続ける"
pactl load-module module-null-sink sink_name=fakemic rate=48000 channels=1 format=s16le >/dev/null
python3 - "$W/tone.raw" <<'PY'
import struct, sys, math
# 1 秒ぶん。1 kHz は 48 kHz でちょうど 48 サンプル周期なので、つなぎ目で途切れない
open(sys.argv[1], 'wb').write(b''.join(
    struct.pack('<h', int(12000 * math.sin(2 * math.pi * 1000 * i / 48000))) for i in range(48000)))
PY
( while :; do cat "$W/tone.raw"; done ) \
  | pacat --playback --device=fakemic --raw --rate=48000 --channels=1 --format=s16le --latency-msec=50 &
PIDS+=($!)

echo "== client と bridge をつなぐ"
CAPTURE=(--capture-cmd "parec --device=fakemic.monitor --raw --rate=16000 --channels=1 --format=s16le --latency-msec=20")
if [ "${E2E_NATIVE:-0}" = 1 ]; then
  # 内蔵の取得（cpal → ALSA → PulseAudio）。既定の録音デバイスの代わりに、代役のマイクを指す
  CAPTURE=()
  export PULSE_SOURCE=fakemic.monitor
  echo "   （内蔵のマイク取得を使う）"
fi
"$BIN" connect --armed --max-seconds 0 --transport "$BIN bridge --sink-name e2e" "${CAPTURE[@]}" \
  </dev/null >"$W/client.log" 2>&1 &
CLIENT=$!
PIDS+=("$CLIENT")
# client の中の PULSE_SOURCE は取得のためのもの。以降の parec には効かせない
unset PULSE_SOURCE
wait_for 15 grep -q '接続しました' "$W/client.log" || fail "client が接続しない"
status_has 'armed=true' || fail "armed になっていない"
wait_for 5 sink_exists e2e || fail "仮想マイクが作られない"
ok "接続と仮想マイク"

echo "== armed: 仮想マイクから録ると、代役のマイクの音が届く"
timeout 4 parec --device=e2e.monitor --raw --rate=16000 --channels=1 --format=s16le >"$W/armed.raw" || true
read -r rms freq < <(analyze "$W/armed.raw")
echo "   rms=$rms 周波数=${freq}Hz"
[ "$rms" -gt 1000 ] || fail "音が届いていない (rms=$rms)"
[ "$freq" -ge 950 ] && [ "$freq" -le 1050 ] || fail "届いた音の周波数が違う (${freq}Hz)"
grep -q '録音要求 → START' "$W/client.log" || fail "START が出ていない"
wait_for 5 grep -q '録音終了 → STOP' "$W/client.log" || fail "STOP が出ていない"
wait_for 5 status_has 'recording=false' || fail "録音が止まらない"
ok "転送と、録音要求の開始・終了"

echo "== disarmed: 録音要求を断り、音は届かない"
"$BIN" disarm >/dev/null
timeout 2 parec --device=e2e.monitor --raw --rate=16000 --channels=1 --format=s16le >"$W/disarmed.raw" || true
read -r rms _ < <(analyze "$W/disarmed.raw")
echo "   rms=$rms"
[ "$rms" -lt 50 ] || fail "disarmed なのに音が届いた (rms=$rms)"
grep -q '録音要求を断りました' "$W/client.log" || fail "断った記録が無い"
ok "disarmed での拒否"

echo "== arm し直すと、また届く"
"$BIN" arm >/dev/null
timeout 3 parec --device=e2e.monitor --raw --rate=16000 --channels=1 --format=s16le >"$W/rearmed.raw" || true
read -r rms _ < <(analyze "$W/rearmed.raw")
echo "   rms=$rms"
[ "$rms" -gt 1000 ] || fail "arm し直しても音が届かない (rms=$rms)"
ok "arm のし直し"

echo "== client が突然終わっても、接続先が元に戻る"
# SIGTERM で即座に落とす。bridge は標準入力の EOF で気づいて後片付けをする
kill "$CLIENT"
wait "$CLIENT" 2>/dev/null || true
gone() { ! sink_exists e2e; }
wait_for 5 gone || fail "仮想マイクが残っている"
ok "後片付け"

echo "すべて通りました"
