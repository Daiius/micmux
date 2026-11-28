#!/usr/bin/env bash
# Linux 上で、client → bridge → 仮想マイクの流れを通しで確かめる。
#
#   ./scripts/e2e-linux.sh <micmux のバイナリ>
#   E2E_NATIVE=1 ./scripts/e2e-linux.sh <--features native でビルドしたバイナリ>
#   E2E_SERVER=pipewire ./scripts/e2e-linux.sh <micmux のバイナリ>
#
# 代役のマイク（音を流し込んだ null sink）から client が取得し、bridge を通って仮想マイクに
# 届いた音を録って照合する。実際のマイクも SSH も使わない（--transport で bridge を直接起動する）。
#
# 利用者の音声サーバーには触れない。専用の音声サーバーを一時ディレクトリに立て、起動したものは
# すべて PID を指定して止める。手元でも CI でも同じように動く。
#
# 音声サーバーは E2E_SERVER で選ぶ。pulseaudio（既定）か pipewire（pipewire-pulse + WirePlumber）。
#
# 要るもの: pactl, pacat, parec, python3 と、
#   pulseaudio のとき: pulseaudio
#   pipewire のとき:   pipewire, pipewire-pulse, wireplumber
# E2E_NATIVE=1 のときは、ALSA から音声サーバーへ繋ぐプラグインも要る
# （pulseaudio なら libasound2-plugins、pipewire なら pipewire-alsa）。
# この 2 つは ALSA の既定デバイスを取り合うので、同じ環境に両方を入れない。

set -euo pipefail

BIN="${1:?usage: $0 <micmux binary>}"
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
[ -x "$BIN" ] || { echo "not executable: $BIN" >&2; exit 2; }
SERVER="${E2E_SERVER:-pulseaudio}"
case "$SERVER" in
  pulseaudio) SERVER_CMDS=(pulseaudio) ;;
  pipewire)   SERVER_CMDS=(pipewire pipewire-pulse wireplumber) ;;
  *) echo "unknown E2E_SERVER: $SERVER (pulseaudio or pipewire)" >&2; exit 2 ;;
esac
for c in "${SERVER_CMDS[@]}" pactl pacat parec python3; do
  command -v "$c" >/dev/null || { echo "$c not found" >&2; exit 2; }
done

W="$(mktemp -d)"
PIDS=()
FAILED=0

# ここから先は、すべて専用の音声サーバーと専用のソケットに向く
export PULSE_RUNTIME_PATH="$W/pulse-rt" PULSE_STATE_PATH="$W/pulse-state" PULSE_SERVER="unix:$W/pulse-rt/native"
export MICMUX_SOCKET="$W/micmux.sock" XDG_STATE_HOME="$W/state" XDG_RUNTIME_DIR="$W/rt"
# 利用者の設定（~/.config/pulse・pipewire・wireplumber）を読ませない
export XDG_CONFIG_HOME="$W/config"
unset PULSE_SINK PULSE_SOURCE
# PipeWire は、XDG_RUNTIME_DIR よりこれらを優先する。残っていると、利用者の PipeWire に繋がってしまう
unset PIPEWIRE_RUNTIME_DIR PIPEWIRE_REMOTE PIPEWIRE_CONFIG_DIR PIPEWIRE_CONFIG_NAME PIPEWIRE_NODE
unset WIREPLUMBER_CONFIG_DIR WIREPLUMBER_DATA_DIR
mkdir -p "$W/pulse-rt" "$W/rt" "$W/config"

cleanup() {
  local p
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
  if [ "$FAILED" -ne 0 ]; then
    echo "---- client.log"; cat "$W/client.log" 2>/dev/null || true
    echo "---- pulse.log";  tail -n 20 "$W/pulse.log" 2>/dev/null || true
    echo "---- pipewire.log"; tail -n 20 "$W/pipewire.log" "$W/wireplumber.log" 2>/dev/null || true
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

echo "== start a dedicated sound server"
# D-Bus を切り離さないと、利用者の音声サーバーと名前が衝突して起動しない
export DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent
if [ "$SERVER" = pipewire ]; then
  # ソケットは XDG_RUNTIME_DIR と PULSE_RUNTIME_PATH の下にできるので、利用者のものとは別になる。
  # 実体のある出力先が無いときの代役（auto_null）は、pipewire-pulse が自分で置く。
  #
  # WirePlumber は、既定では実際のデバイス（ALSA・Bluetooth・カメラ）を探して掴む。試験用の
  # インスタンスがそれをすると、利用者の音声サーバーとデバイスを取り合う。デバイスの監視を止める。
  # この書き方（wireplumber.profiles）は 0.5 以降のもの。0.4 は設定の形式が違うので、回さない
  case "$(wireplumber --version 2>/dev/null | sed -n 's/^Linked with libwireplumber //p')" in
    0.[0-4].*|"") fail "E2E_SERVER=pipewire needs WirePlumber 0.5 or later" ;;
  esac
  mkdir -p "$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d"
  cat >"$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d/90-e2e-no-hardware.conf" <<'CONF'
wireplumber.profiles = {
  main = {
    hardware.audio = disabled
    hardware.bluetooth = disabled
    hardware.video-capture = disabled
  }
}
CONF
  # Bluetooth は system bus を通るので、こちらも切り離す
  export DBUS_SYSTEM_BUS_ADDRESS=unix:path=/nonexistent
  pipewire >"$W/pipewire.log" 2>&1 &
  PIDS+=($!)
  wait_for 10 test -S "$XDG_RUNTIME_DIR/pipewire-0" || fail "pipewire did not start"
  wireplumber >"$W/wireplumber.log" 2>&1 &
  PIDS+=($!)
  pipewire-pulse >"$W/pulse.log" 2>&1 &
  PIDS+=($!)
else
  # module-always-sink: 実体のある出力先が無いときの代役（auto_null）を置く。ヘッドレスのサーバーと同じ状態にする
  pulseaudio -n \
    -L "module-native-protocol-unix socket=$W/pulse-rt/native" -L module-always-sink \
    --daemonize=no --exit-idle-time=-1 --use-pid-file=no --log-target="file:$W/pulse.log" &
  PIDS+=($!)
fi
wait_for 10 pactl info || fail "sound server did not start"
pactl info | grep -q "$W" || fail "not connected to the dedicated sound server"
if [ "$SERVER" = pipewire ]; then
  pactl info | grep -q 'Server Name:.*PipeWire' || fail "the sound server is not PipeWire"
  # 代役の出力先は、WirePlumber が動き出してから現れる
  wait_for 10 sink_exists auto_null || fail "the placeholder sink did not appear"
  # 念のための歯止め。監視を止めたのに実際のデバイスが見えていたら、先へ進まない
  # （既定の出力先へ鳴らす試験が、本物のスピーカーから鳴ってしまう）
  sleep 0.5
  [ "$(pactl list short sinks | cut -f2)" = auto_null ] \
    || fail "the dedicated sound server sees real devices: $(pactl list short sinks | cut -f2 | tr '\n' ' ')"
fi
ok "sound server ($(pactl info | sed -n 's/^Server Name: //p'))"

echo "== connect client and bridge"
CAPTURE=(--capture-cmd "parec --device=fakemic.monitor --raw --rate=16000 --channels=1 --format=s16le --latency-msec=20")
if [ "${E2E_NATIVE:-0}" = 1 ]; then
  # 内蔵の取得（cpal → ALSA → 音声サーバー）。既定の録音デバイスの代わりに、代役のマイクを指す。
  # PULSE_SOURCE が効くのは PulseAudio の ALSA プラグインだけ。PipeWire のものは見ないので、
  # 下で代役のマイクを作ったあと、既定の録音デバイスにもする
  CAPTURE=()
  export PULSE_SOURCE=fakemic.monitor
  echo "   (using built-in capture)"
fi
"$BIN" connect --armed --max-seconds 0 --transport "$BIN bridge --sink-name e2e" "${CAPTURE[@]}" \
  </dev/null >"$W/client.log" 2>&1 &
CLIENT=$!
PIDS+=("$CLIENT")
# client の中の PULSE_SOURCE は取得のためのもの。以降の parec には効かせない
unset PULSE_SOURCE
wait_for 15 grep -q 'connected to' "$W/client.log" || fail "client did not connect"
status_has 'armed=true' || fail "not armed"
wait_for 5 sink_exists e2e || fail "virtual mic was not created"
ok "connection and virtual mic"

echo "== the virtual mic is not the default sink"
[ "$(pactl get-default-sink)" != e2e ] || fail "the virtual mic is the default sink"
ok "default sink is $(pactl get-default-sink)"

echo "== play a continuous 1 kHz tone into the fake mic"
# bridge より後に作る。先に作ると、これが既定の出力先になってしまい、下の混入の試験にならない
FAKEMIC_MODULE="$(pactl load-module module-null-sink sink_name=fakemic rate=48000 channels=1 format=s16le)"
if [ "${E2E_NATIVE:-0}" = 1 ]; then pactl set-default-source fakemic.monitor; fi
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

echo "== armed: recording from the virtual mic yields the fake mic's tone"
timeout 4 parec --device=e2e.monitor --raw --rate=16000 --channels=1 --format=s16le >"$W/armed.raw" || true
read -r rms freq < <(analyze "$W/armed.raw")
echo "   rms=$rms freq=${freq}Hz"
[ "$rms" -gt 1000 ] || fail "no audio arrived (rms=$rms)"
[ "$freq" -ge 950 ] && [ "$freq" -le 1050 ] || fail "wrong frequency (${freq}Hz)"
grep -q 'recording requested → START' "$W/client.log" || fail "no START"
wait_for 5 grep -q 'recording ended → STOP' "$W/client.log" || fail "no STOP"
wait_for 5 status_has 'recording=false' || fail "recording did not stop"
ok "forwarding, and start/end of recording requests"

echo "== disarmed: requests are denied, no audio arrives, and sounds played on the remote side do not leak in"
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
[ "$rms" -lt 50 ] || fail "audio arrived while disarmed; playback on this host may have leaked in (rms=$rms)"
grep -q 'denied a recording request' "$W/client.log" || fail "denial not logged"
ok "denial while disarmed"

echo "== re-arming makes audio arrive again"
"$BIN" arm >/dev/null
timeout 3 parec --device=e2e.monitor --raw --rate=16000 --channels=1 --format=s16le >"$W/rearmed.raw" || true
read -r rms _ < <(analyze "$W/rearmed.raw")
echo "   rms=$rms"
[ "$rms" -gt 1000 ] || fail "no audio after re-arming (rms=$rms)"
ok "re-arm"

echo "== connect a second bridge with a differently named virtual mic"
# 代役のマイクは、ここで片付ける。出力先が micmux のものだけ、という実運用と同じ状態にするため
kill "$TONE" 2>/dev/null || true
wait "$TONE" 2>/dev/null || true
pactl unload-module "$FAKEMIC_MODULE"
# 操作用ソケットを分けて、2 つ目の client を disarmed で動かす。このとき既定の出力先は 1 つ目の捨て先
MICMUX_SOCKET="$W/second.sock" "$BIN" connect --transport "$BIN bridge --sink-name e2e2" \
  --capture-cmd "cat /dev/zero" </dev/null >"$W/client2.log" 2>&1 &
CLIENT2=$!
PIDS+=("$CLIENT2")
wait_for 15 grep -q 'connected to' "$W/client2.log" || fail "second client did not connect"
wait_for 5 sink_exists e2e2 || fail "second virtual mic was not created"
ok "second connection"

echo "== the remote side is restored when the client exits abruptly"
# SIGTERM で即座に落とす。bridge は標準入力の EOF で気づいて後片付けをする
kill "$CLIENT"
wait "$CLIENT" 2>/dev/null || true
gone() { ! sink_exists e2e && ! sink_exists e2e_discard; }
wait_for 5 gone || fail "the virtual mic or the discard sink is left behind"
ok "cleanup"

echo "== when the first bridge exits, the remaining virtual mic does not become the default sink"
# 1 つ目が捨て先を消すと、残る sink は 2 つ目の仮想マイクだけになり、音声サーバーがそれを既定に選ぶ。
# 2 つ目の bridge がそれに気づいて、自分の捨て先に替える
moved() { [ "$(pactl get-default-sink)" = e2e2_discard ]; }
wait_for 5 moved || fail "default sink is still $(pactl get-default-sink)"
( while cat "$W/noise.raw"; do :; done 2>/dev/null ) \
  | pacat --playback --raw --rate=48000 --channels=1 --format=s16le --latency-msec=50 &
NOISE=$!
PIDS+=("$NOISE")
sleep 0.5
timeout 2 parec --device=e2e2.monitor --raw --rate=16000 --channels=1 --format=s16le >"$W/second.raw" || true
kill "$NOISE" 2>/dev/null || true
read -r rms _ < <(analyze "$W/second.raw")
echo "   rms=$rms"
[ "$rms" -lt 50 ] || fail "playback on this host leaked into the remaining virtual mic (rms=$rms)"
kill "$CLIENT2"
wait "$CLIENT2" 2>/dev/null || true
gone2() { ! sink_exists e2e2 && ! sink_exists e2e2_discard; }
wait_for 5 gone2 || fail "the second virtual mic or its discard sink is left behind"
ok "default sink re-check, and cleanup"

echo "== the default sink is left alone when it cannot be checked"
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
wait_for 15 grep -q 'connected to' "$W/client3.log" || fail "third client did not connect"
[ "$(pactl get-default-sink)" = speaker ] || fail "default sink was changed ($(pactl get-default-sink))"
! sink_exists e2e3_discard || fail "created a discard sink without being able to check"
grep -q 'cannot check the default sink' "$W/client3.log" || fail "no log saying the check failed"
kill "$CLIENT3"
wait "$CLIENT3" 2>/dev/null || true
gone3() { ! sink_exists e2e3; }
wait_for 5 gone3 || fail "third virtual mic is left behind"
[ "$(pactl get-default-sink)" = speaker ] || fail "default sink changed after exit"
ok "no changes when the check fails"

echo "== info: time from recording request to first audio"
grep -o 'first audio [0-9]* ms after START' "$W/client.log" | sed 's/^/   /' || true

echo "all passed"
