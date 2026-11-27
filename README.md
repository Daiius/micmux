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

音声は端末の入出力とは別の経路（専用の SSH 接続、または多重化した別チャネル）で送るので、
tmux やターミナルエミュレーターには依存しない。

仕様と設計の判断は [`prd/`](./prd/) に、決定の経緯は [`prd/_grilling/decisions.md`](./prd/_grilling/decisions.md) にある。
開発を手伝うエージェント向けの手引きは [`AGENTS.md`](./AGENTS.md)。

## 現状

2026-11-27 時点。macOS から Debian へ、実際の SSH 越しに Claude Code へ音声入力できることを確認済み。

| 項目 | 状態 |
|---|---|
| Claude Code 2.1.332 が SSH + tmux 内で仮想マイクから録音し、文字起こしできる | 実測で確認 |
| 録音の開始・終了を PulseAudio の録音ストリームの有無で検出できる | 実測で確認 |
| disarmed では録音要求を断る / arm で受け付ける | 実測で確認 |
| 接続元の異常終了・端末ごとの切断・bridge への SIGTERM で接続先が元に戻る | 実測で確認 |
| macOS (Core Audio) でのマイク取得 | 実測で確認。Linux (ALSA) でも動作確認 |
| 実際の SSH 越しの転送と、日本語の文字起こし | 実測で確認 |
| `LocalCommand` からの自動起動、裏での動作 | macOS で実測で確認 |
| `--mux` による接続の相乗り（認証 1 回）、`Match exec` からの起動 | macOS と指紋認証の鍵で実測で確認 |
| 裏で動かしたときのマイクの許可の引き継ぎ | macOS で実測で確認（端末から直接の場合と同じ音量が取れる） |
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

### ssh に連動して自動で起動する

`~/.ssh/config` に 2 行足すと、`ssh dev-server` したときに micmux が裏で立ち上がり、
その SSH を閉じると終了する。別の端末で `micmux connect` を実行する必要がなくなる。

```
Host dev-server
    PermitLocalCommand yes
    LocalCommand micmux connect %n --set-default --background --watch-pid $PPID
```

- 接続先で micmux が PATH に無い場合は `--remote-cmd '~/.cargo/bin/micmux'` を足す
  （非対話の SSH ではログインシェルの設定が読まれないことがある）。
- 同じホストへ SSH を複数開いた場合、2 本目以降は動いている micmux に相乗りする。
  すべての SSH が終わった時点で micmux も終わる。ただし、端末から `--watch-pid` 無しで
  手で起動した micmux に相乗りした場合は、その SSH が終わっても micmux は動き続ける。
- 起動直後は disarmed。録音の許可は `micmux arm` / `micmux toggle` で切り替える
  （キーバインドに割り当てておくとよい）。裏で動いているあいだは、録音要求を断っても
  ログにしか出ないので気づきにくい。最初から許可するなら `--armed` を足す。
- 裏で動いているあいだの表示は `~/.local/state/micmux/client.log` に出る（本人だけが読める権限で作る）。
  起動時に 1 MB を超えていたら `client.log.1` に退避して作り直す。`XDG_STATE_HOME` も `HOME` も無い環境では
  ログの置き場所が決まらないので、`--log` で指定しない限り起動しない。
- 端末が無いのでパスフレーズを尋ねられない。鍵は ssh-agent に載せておく。
- `LocalCommand` は `git` や `scp` がそのホストへ繋ぐときにも走る。その場合 micmux は
  接続が終わるのと同時に終了する。

### 認証を 1 回で済ませる

micmux は端末用とは別の SSH 接続を張るので、そのままでは認証が 2 回要る。
接続のたびに指紋認証やパスフレーズの入力がある鍵では、`--mux` で端末用の接続に相乗りさせると
1 回で済む（SSH の多重化を使う）。

```
Host dev-server
    ControlMaster auto
    ControlPath ~/.ssh/cm-%C
    ControlPersist 1m

Match originalhost dev-server exec "micmux connect %n --mux --background --watch-pid $PPID --set-default"
```

- `LocalCommand` の代わりに `Match ... exec` で起動する。相乗りした 2 本目以降の SSH では
  `LocalCommand` が走らないため。上の `LocalCommand` の設定と併用する必要はない。
- `Match originalhost` は、コマンドラインで指定した名前が一致したときだけ発動する。
  IP アドレスや別の名前で接続すると micmux は起動しない。
- micmux は、端末用の SSH が認証を終えるのを待ってから接続する。
- `ControlMaster`（`auto` など）、`ControlPath`、`ControlPersist` は必須。どれかが欠けていると
  micmux は起動時にそう伝えて終わる。`ControlPersist` により、最後の SSH を閉じたあとも
  指定した時間は接続が保たれる（そのあいだの再接続は認証なしで通る）。
- 音声と端末の通信が同じ TCP 接続を通る。大量の出力を流している最中は音声が遅れうる。
  通常の使い方では、接続を分けた場合との差は実測で見られなかった（後述）。

うまく動かないときは、接続元で次を実行すると状況をまとめて表示する。
micmux の版、動いているプロセス、ログの末尾、ssh の設定、端末なしでの接続可否、
マイクから音が取れるか（端末から直接と、切り離した状態の両方）を出す。

```sh
micmux debug dev-server
```

`connect` に `--remote-cmd` を付けている場合は、`debug` にも同じものを付ける。

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
| `--max-seconds <n>` | 連続録音時間の上限（既定 120 秒）。達すると disarm する。`0` で上限なし |
| `--remote-cmd <path>` | 接続先での micmux の場所。非対話の SSH では `~/.cargo/bin` が PATH に無いことがある |
| `--ssh-arg <arg>` | ssh に渡す追加の引数 |
| `--capture-cmd <cmd>` | マイク取得を外部コマンドに任せる。s16le モノラルを stdout に出すもの |
| `--transport <cmd>` | ssh の代わりに使う接続コマンド |
| `--background` | 端末から切り離して裏で動く |
| `--watch-pid <pid>` | 指定したプロセスが終わったら自分も終わる |
| `--mux` | 端末用の SSH 接続に相乗りする |

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

接続元がスリープやネットワークの切り替えで消えると、TCP が半開きのまま残り、bridge には
切断が伝わらない（sshd の既定では何時間も）。そこで client は 10 秒ごとに生存確認を送り、
bridge は 30 分間何も届かなければ後片付けをして終わる。短いスリープから復帰した接続元が
そのまま使い続けられるよう、この時間は長めにしてある。sshd 側で `ClientAliveInterval` を
設定しておくと、ssh の接続そのものも早く片付く。

同じ名前の仮想マイクを使う bridge は 1 つだけ動く。前の bridge が残っているところへ新しい
bridge が起動すると、前の bridge を終わらせ、その後片付けが済んでから仮想マイクを作る。

### 接続元: client

`recording = armed && connected && requested`。bridge からの START は要求でしかなく、
armed でなければ断る。待機中は入力デバイスを開かない。

接続先は無条件には信頼しない。

- 連続録音時間が `--max-seconds` に達したら録音を止めて disarm する。続けるには `micmux arm` が要る。
  録音中に START が届き直しても、時間は数え直さない。
- 接続先から届いた文字列（仮想マイクの名前、エラー、ssh の標準エラー出力）は、端末の制御文字を
  取り除き、1 行の長さと 1 分あたりの行数を制限してから表示する。ssh 自身のメッセージもここを通る
  （パスフレーズや host key の確認は ssh が端末に直接尋ねるので影響しない）。

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

## 実測値

macOS (Apple シリコン、内蔵マイク) から Debian へ、往復 約 10 ms の回線で測った。

| 区間 | 値 |
|---|---|
| 録音要求を受けてからマイクが最初の音声を出すまで | 105〜110 ms（起動直後の初回のみ 約 140 ms） |
| 同じ区間を接続先から見た値（通信の往復を含む） | 110〜118 ms |
| 接続先で仮想マイクへ流し込む際のバッファ | 60 ms（設定値） |
| 転送量 | 約 32 KB/秒（録音中のみ） |

- 接続を分けた場合と `--mux` で相乗りした場合とで、上の値に差は無かった。
- 27 秒間の連続録音でも、受信した音声の長さは録音時間と 0.1 秒差で、遅れは溜まらなかった。
- 文字起こしにかかる時間は micmux の外で、ここには含まれない。

これらの数字は、裏で動かしたときのログと `micmux status` の `rtt` で確認できる。

## 既知の制限

- **発話の冒頭が欠けうる。** 録音要求が来てからマイクを開くので、開くまでの音は届かない。
  START から最初の音声が届くまでの実測値は、macOS (Core Audio) で約 110 ms、
  Linux (ALSA → PulseAudio) で 0.6〜1.0 秒。macOS からなら実用上ほぼ気にならないが、
  Linux を接続元にする場合はキーを押してひと呼吸おいてから話す必要がある。
- 送信が詰まっている最中は、取得スレッドの停止が送信の完了待ちになる。接続先が受信を止めると、
  その間は `micmux disarm` や `q` が効かず（`timeout` と返る）、マイクも開いたままになる。
  止めた後の音声が送られることはない。抜け出すには Ctrl+C か、micmux のプロセスを kill する。
- 連続録音時間の上限は、1 回の録音の長さを抑えるもの。armed のあいだは、接続先が録音の終了と
  開始を繰り返せば、上限より短い録音を何度でも始められる。録音させたくない間は disarm しておく。
- 接続元が Linux で、`XDG_RUNTIME_DIR` も `TMPDIR` も無い環境では、操作用ソケットを
  `/tmp/micmux-<ユーザー名>.sock` に置く。同じマシンの別の利用者が先にこの名前のファイルを作ると、
  micmux が起動できない、相乗りしたつもりで何もせずに終わる、あるいは操作用ソケット無しで動いて
  `micmux disarm` が届かない、といったことが起こりうる。macOS ではユーザーごとの一時ディレクトリを使うので起こらない。
- 接続先で `XDG_RUNTIME_DIR` が無い環境では、bridge の排他用のファイルを `/tmp` に置く。同じマシンの
  別の利用者が先にこの名前のファイルを作ると、排他が効かなくなり、前の bridge が残っていた場合に
  仮想マイクが壊れうる（別の利用者のプロセスを止めてしまうことはない）。通常の SSH ログインでは設定されている。
- 接続先のサウンドカードが 1 枚も無く `arecord` も無い環境では、Claude Code が録音方法を
  見つけられない。`alsa-utils` を入れる。
- 再接続はしない。切れたら `micmux connect` をやり直す。
- 同時に動かせる client は 1 つ。接続先では後から接続した方が優先され、前の接続は切れる。

## 開発

```sh
cargo test
```

Linux を接続元にして内蔵のマイク取得を使う場合は `--features native` でビルドする（`libasound2-dev` が必要）。
