# micmux

> On-demand microphone forwarding over SSH, independent of terminals and applications.

SSH 接続元のマイクを、接続先の仮想マイクとして使えるようにする。
接続先のアプリが録音を始めた瞬間だけ接続元のマイクが起動し、録音が終わると止まる。

```text
接続元 (macOS)                         接続先 (Debian)
┌──────────────────────┐   SSH    ┌───────────────────────────────────┐
│ マイク → micmux connect ├─────────┤ micmux bridge → 仮想マイク → アプリ │
└──────────────────────┘ stdin/out └───────────────────────────────────┘
        ← START / STOP（録音要求）
        → PCM（armed のときだけ）
```

端末用の SSH 接続とは別の接続を張るので、tmux やターミナルエミュレーターには依存しない。

## 現状

2026-11-27 時点。**接続先側は実機で確認済み、接続元の macOS 側は未確認。**

| 項目 | 状態 |
|---|---|
| Claude Code 2.1.332 が SSH + tmux 内で仮想マイクから録音し、文字起こしできる | 実測で確認 |
| 録音の開始・終了を PulseAudio の録音ストリームの有無で検出できる | 実測で確認 |
| disarmed では録音要求を断る / arm で受け付ける | 実測で確認 |
| 接続元の異常終了・端末ごとの切断・bridge への SIGTERM で接続先が元に戻る | 実測で確認 |
| cpal によるマイク取得のコード | Linux (ALSA) で動作確認。**macOS (Core Audio) では未ビルド・未実行** |
| 本物の SSH 越しの転送 | **未確認**（同一マシン上で `--transport` を使って確認した） |
| pipewire-pulse での動作 | 未確認（PulseAudio 17.0 で確認） |
| Codex CLI | 未確認。0.162.1 の Linux 版バイナリには録音の実装が見当たらない（後述） |

## セットアップ

### 接続先 (Debian)

音声基盤を入れる。変更前に `--check` で現状を見られる。

```sh
./scripts/setup-debian-server.sh --check
./scripts/setup-debian-server.sh
```

micmux をビルドして PATH に置く。接続先のビルドに音声関係の開発ヘッダは要らない。

```sh
cargo install --path .
micmux doctor
```

### 接続元 (macOS)

```sh
cargo install --path .
```

初回の録音時に、ターミナルアプリに対するマイクのアクセス許可を macOS が尋ねる。

## 使い方

接続元で:

```sh
micmux connect dev-server --set-default
```

起動直後は **disarmed**（録音要求をすべて断る）。Enter で armed と disarmed を切り替える。
別の端末やキーバインドからは次のコマンドで操作できる。

```sh
micmux arm
micmux disarm
micmux toggle
micmux status --json
```

接続先では、いつも通り SSH + tmux の中でアプリを使う。Claude Code なら `/voice` で音声入力を有効にし、
スペースキーを押しているあいだ録音される。

### 録音デバイスの選ばれ方

`--set-default` を付けると、接続中だけ仮想マイクを接続先の既定の録音デバイスにする（切断時に戻す）。
既定を変えたくない場合は付けずに、使いたいアプリにだけ環境変数で指定する。

```sh
PULSE_SOURCE=micmux.monitor claude
```

### 主なオプション

| オプション | 意味 |
|---|---|
| `--armed` | 最初から録音を許可する |
| `--max-seconds <n>` | 連続録音時間の上限（既定 120 秒） |
| `--remote-cmd <path>` | 接続先での micmux の場所。非対話の SSH では `~/.cargo/bin` が PATH に無いことがある |
| `--ssh-arg <arg>` | ssh に渡す追加の引数 |
| `--capture-cmd <cmd>` | マイク取得を外部コマンドに任せる。s16le モノラルを stdout に出すもの |
| `--transport <cmd>` | ssh の代わりに使う接続コマンド |

内蔵のマイク取得がうまく動かない場合は、SoX で代用できる。

```sh
micmux connect dev-server --capture-cmd 'rec -q -t raw -r 16000 -e signed -b 16 -c 1 -'
```

## 仕組み

### 接続先: bridge

1. null sink を 1 つ作る。その monitor source が仮想マイクになる。
2. `pactl subscribe` で録音ストリームの増減を監視する。イベントをそのまま START / STOP に
   変換せず、イベントのたびに「仮想マイクに繋がっていて cork されていない録音ストリームの数」を
   取り直して状態を決める。
3. 数が 0 → 1 以上になったら START、0 に戻って 300 ms 経ったら STOP を送る。
4. 届いた PCM を `pacat` で null sink に流す。`pacat` は録音セッションごとに起動し直すので、
   前のセッションの残りが次に混ざらない。

libpulse にはリンクせず、`pactl` と `pacat` を子プロセスとして使う。

### 接続元: client

`recording = armed && connected && requested`。bridge からの START は要求でしかなく、
armed でなければ断る。待機中は入力デバイスを開かない。

48 kHz のような 16 kHz の整数倍のレートは 16 kHz に落としてから送る（32 KB/秒）。
それ以外のレートはそのまま送り、接続先のサウンドサーバーに変換を任せる。

### プロトコル

`type (u8) | length (u32 LE) | payload` のフレームを SSH の標準入出力に流す。
詳細は `src/protocol.rs`。

## 調べてわかったこと

### Claude Code の録音経路（バイナリの解析と実測）

- Linux では次の順で録音方法を選ぶ。
  1. 内蔵の録音モジュール（cpal 0.15.3、ALSA を直接使う）。`/proc/asound/cards` にカードがあるとき
  2. `arecord`
  3. SoX の `rec`
- **PulseAudio を直接は使わない。** ALSA の既定デバイスを開くので、ALSA → PulseAudio の
  プラグイン（`libasound2-plugins`）が橋渡しをする。Debian の `pulseaudio` パッケージは、
  サーバーが動いていれば ALSA の既定をそちらへ向ける設定を入れる。
- 録音ストリームはキーを押すと作られ、離すと破棄される。常時開きっぱなしにはしない。
- 本番の録音の直前に、ごく短い試し開きが 1 回ある。STOP の猶予 (300 ms) はこれを
  1 セッションにまとめるためのもの。
- 要求する形式は 48 kHz / float32 / モノラル。
- SSH 越しであること自体は音声機能を無効にする条件になっていない。

### Codex CLI

0.162.1 の Linux 版（musl の静的リンク）には ALSA や PulseAudio を呼ぶ痕跡が無かった。
Linux 版では録音できない可能性が高いが、実際に起動しては確かめていない。

## 既知の制限

- **発話の冒頭が欠ける。** 録音要求が来てからマイクを開くので、開くまでの音は届かない。
  Linux 上の測定では START から最初の音声まで 0.6〜1.0 秒かかった（macOS では未測定）。
  キーを押してひと呼吸おいてから話す必要がある。
- 送信が詰まっている最中は、取得スレッドの停止が送信の完了待ちになる。
- 接続先のサウンドカードが 1 枚も無く `arecord` も無い環境では、Claude Code が録音方法を
  見つけられない。`alsa-utils` を入れる。
- 再接続はしない。切れたら `micmux connect` をやり直す。
- 同時に動かせる client は 1 つ。

## 開発

```sh
cargo test
```

Linux を接続元にして内蔵のマイク取得を使う場合は `--features native` でビルドする（`libasound2-dev` が必要）。
