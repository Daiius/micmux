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
# module-always-sink: 実体のある出力先が無いときの代役（auto_null）を置く。ヘッドレスのサーバーと同じ状態にする
DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent pulseaudio -n \
  -L "module-native-protocol-unix socket=$W/pulse-rt/native" -L module-always-sink \
  --daemonize=no --exit-idle-time=-1 --use-pid-file=no --log-target="file:$W/pulse.log" &
PIDS+=($!)
wait_for 10 pactl info || fail "音声サーバーが起動しない"
pactl info | grep -q "$W" || fail "専用の音声サーバーに繋がっていない"
ok "音声サーバー"

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

echo "== 仮想マイクが、既定の出力先になっていない"
[ "$(pactl get-default-sink)" != e2e ] || fail "仮想マイクが既定の出力先になっている"
ok "既定の出力先は $(pactl get-default-sink)"

echo "== 代役のマイクに 1 kHz の音を流し続ける"
# bridge より後に作る。先に作ると、これが既定の出力先になってしまい、下の混入の試験にならない
FAKEMIC_MODULE="$(pactl load-module module-null-sink sink_name=fakemic rate=48000 channels=1 format=s16le)"
python3 - "$W/tone.raw" <<'PY'
import struct, sys, math
# 1 秒ぶん。1 kHz は 48 kHz でちょうど 48 サンプル周期なので、つなぎ目で途切れない
open(sys.argv[1], 'wb').write(b''.join(
    struct.pack('<h', int(12000 * math.sin(2 * math.pi * 1000 * i / 48000))) for i in range(48000)))
PY
# cat が失敗したら（pacat が止まった、ファイルが消えた）ループも終わる。while : だと空回りして残る
( while cat "$W/tone.raw"; do :; done 2>/dev/null ) \
  | pacat --playback --device=fakemic --raw --rate=48000 --channels=1 --format=s16le --latency-msec=50 &
TONE=$!
PIDS+=("$TONE")

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

echo "== disarmed: 録音要求を断り、音は届かない。接続先で鳴らした音も混ざらない"
"$BIN" disarm >/dev/null
# 出力先を指定せずに鳴らす（＝既定の出力先へ）。仮想マイクが既定になっていると、ここで混ざる
python3 - "$W/noise.raw" <<'PY'
import struct, sys, math
open(sys.argv[1], 'wb').write(b''.join(
    struct.pack('<h', int(12000 * math.sin(2 * math.pi * 3000 * i / 48000))) for i in range(48000)))
PY
( while cat "$W/noise.raw"; do :; done 2>/dev/null ) \
  | pacat --playback --raw --rate=48000 --channels=1 --format=s16le --latency-msec=50 &
NOISE=$!
PIDS+=("$NOISE")
sleep 0.5
timeout 2 parec --device=e2e.monitor --raw --rate=16000 --channels=1 --format=s16le >"$W/disarmed.raw" || true
read -r rms _ < <(analyze "$W/disarmed.raw")
echo "   rms=$rms"
kill "$NOISE" 2>/dev/null || true
[ "$rms" -lt 50 ] || fail "disarmed なのに音が届いた。接続先で鳴らした音が混ざった可能性もある (rms=$rms)"
grep -q '録音要求を断りました' "$W/client.log" || fail "断った記録が無い"
ok "disarmed での拒否"

echo "== arm し直すと、また届く"
"$BIN" arm >/dev/null
timeout 3 parec --device=e2e.monitor --raw --rate=16000 --channels=1 --format=s16le >"$W/rearmed.raw" || true
read -r rms _ < <(analyze "$W/rearmed.raw")
echo "   rms=$rms"
[ "$rms" -gt 1000 ] || fail "arm し直しても音が届かない (rms=$rms)"
ok "arm のし直し"

echo "== 別の名前の仮想マイクを使う bridge を、もう 1 つつなぐ"
# 代役のマイクは、ここで片付ける。出力先が micmux のものだけ、という実運用と同じ状態にするため
kill "$TONE" 2>/dev/null || true
wait "$TONE" 2>/dev/null || true
pactl unload-module "$FAKEMIC_MODULE"
# 操作用ソケットを分けて、2 つ目の client を disarmed で動かす。このとき既定の出力先は 1 つ目の捨て先
MICMUX_SOCKET="$W/second.sock" "$BIN" connect --transport "$BIN bridge --sink-name e2e2" \
  --capture-cmd "cat /dev/zero" </dev/null >"$W/client2.log" 2>&1 &
CLIENT2=$!
PIDS+=("$CLIENT2")
wait_for 15 grep -q '接続しました' "$W/client2.log" || fail "2 つ目の client が接続しない"
wait_for 5 sink_exists e2e2 || fail "2 つ目の仮想マイクが作られない"
ok "2 つ目の接続"

echo "== client が突然終わっても、接続先が元に戻る"
# SIGTERM で即座に落とす。bridge は標準入力の EOF で気づいて後片付けをする
kill "$CLIENT"
wait "$CLIENT" 2>/dev/null || true
gone() { ! sink_exists e2e && ! sink_exists e2e_discard; }
wait_for 5 gone || fail "仮想マイクか、出力の捨て先が残っている"
ok "後片付け"

echo "== 1 つ目が先に終わっても、残った仮想マイクは既定の出力先にならない"
# 1 つ目が捨て先を消すと、残る sink は 2 つ目の仮想マイクだけになり、音声サーバーがそれを既定に選ぶ。
# 2 つ目の bridge がそれに気づいて、自分の捨て先に替える
moved() { [ "$(pactl get-default-sink)" = e2e2_discard ]; }
wait_for 5 moved || fail "既定の出力先が $(pactl get-default-sink) のまま"
( while cat "$W/noise.raw"; do :; done 2>/dev/null ) \
  | pacat --playback --raw --rate=48000 --channels=1 --format=s16le --latency-msec=50 &
NOISE=$!
PIDS+=("$NOISE")
sleep 0.5
timeout 2 parec --device=e2e2.monitor --raw --rate=16000 --channels=1 --format=s16le >"$W/second.raw" || true
kill "$NOISE" 2>/dev/null || true
read -r rms _ < <(analyze "$W/second.raw")
echo "   rms=$rms"
[ "$rms" -lt 50 ] || fail "接続先で鳴らした音が、残った仮想マイクに混ざった (rms=$rms)"
kill "$CLIENT2"
wait "$CLIENT2" 2>/dev/null || true
gone2() { ! sink_exists e2e2 && ! sink_exists e2e2_discard; }
wait_for 5 gone2 || fail "2 つ目の仮想マイクか、捨て先が残っている"
ok "既定の出力先の見直しと、後片付け"

echo "== 既定の出力先を確かめられないときは、書き換えない"
# 実体のある出力先の代わりに、スピーカー役の sink を既定にしておく
pactl load-module module-null-sink sink_name=speaker >/dev/null
pactl set-default-sink speaker
# get-default-sink だけ失敗する pactl。古い音声サーバーなど、確かめる手段が無い環境の代わり
mkdir -p "$W/fakebin"
REAL_PACTL="$(command -v pactl)"
cat >"$W/fakebin/pactl" <<SH
#!/bin/sh
[ "\$1" = get-default-sink ] && exit 1
exec "$REAL_PACTL" "\$@"
SH
chmod +x "$W/fakebin/pactl"
MICMUX_SOCKET="$W/third.sock" PATH="$W/fakebin:$PATH" "$BIN" connect \
  --transport "$BIN bridge --sink-name e2e3" --capture-cmd "cat /dev/zero" </dev/null >"$W/client3.log" 2>&1 &
CLIENT3=$!
PIDS+=("$CLIENT3")
wait_for 15 grep -q '接続しました' "$W/client3.log" || fail "3 つ目の client が接続しない"
[ "$(pactl get-default-sink)" = speaker ] || fail "既定の出力先が書き換えられた ($(pactl get-default-sink))"
! sink_exists e2e3_discard || fail "確かめられないのに、捨て先を作った"
grep -q '既定の出力先を確かめられない' "$W/client3.log" || fail "確かめられなかった旨のログが無い"
kill "$CLIENT3"
wait "$CLIENT3" 2>/dev/null || true
gone3() { ! sink_exists e2e3; }
wait_for 5 gone3 || fail "3 つ目の仮想マイクが残っている"
[ "$(pactl get-default-sink)" = speaker ] || fail "終了後に、既定の出力先が変わっていた"
ok "確かめられないときは触らない"

echo "== 参考: 録音要求から最初の音声が届くまで"
grep -o 'START から最初の音声まで [0-9]* ms' "$W/client.log" | sed 's/^/   /' || true

echo "すべて通りました"
