# 02. アーキテクチャ

構成要素、通信路、起動の仕方、動いているあいだの排他と後片付けをまとめる。
フレームの中身は [03](./03-protocol.md)、音声の扱いは [04](./04-audio.md)。

---

## 1. 構成要素

単一のバイナリ `micmux` が、サブコマンドで役割を変える。

| 役割 | サブコマンド | 動く場所 | 主なソース |
|---|---|---|---|
| client | `micmux connect <host>` | 接続元 | `client.rs`・`control.rs`・`ssh.rs`・`capture.rs` |
| bridge | `micmux bridge` | 接続先（client が SSH 越しに起動する） | `bridge.rs` |
| 操作 | `micmux arm` / `disarm` / `toggle` / `status` | 接続元 | `control.rs` |
| 点検 | `micmux doctor`（接続先）/ `micmux debug`（接続元） | それぞれ | `main.rs`・`debug.rs` |

- **依存 crate は `libc` と `cpal` だけ**にする。引数の解釈・設定の読み込み・非同期ランタイム・ログ・JSON は
  標準ライブラリで足りる規模であり、依存を増やさない（決定。[decisions](./_grilling/decisions.md)）。
- 並行処理は**スレッドとチャネル**で書く。接続ごとのスレッドは数本で、非同期ランタイムは要らない。
- macOS 固有・Linux 固有の依存は、Cargo の target 別の依存と `cfg` で分ける。接続先のビルドに、
  音声関係の開発ヘッダを要求しない。

## 2. 通信路

### 2.1 SSH の標準入出力を使う（決定）

client が `ssh` を子プロセスとして起動し、接続先で `micmux bridge` を実行させる。
その標準入力と標準出力が、そのまま client と bridge の通信路になる。

```
ssh -T -o Compression=no -o ControlMaster=no [-o ControlPath=none] \
    -o ServerAliveInterval=10 -o ServerAliveCountMax=3 -o PermitLocalCommand=no \
    [-o BatchMode=yes] <host> -- micmux bridge [...]
```

- **待ち受けポートが要らない。** リバースフォワーディングも HTTP サーバーも使わない。
- **SSH の認証と暗号化をそのまま使う。** `~/.ssh/config` の設定（別名・鍵・踏み台）も効く。
- **bridge の寿命が SSH の接続に結びつく。** 接続先に常駐サービスを置かない。
- bridge の標準出力はプロトコル専用で、ログは標準エラーに出す。標準エラーは SSH を通って client に届き、
  client が量と中身を制限してから表示する（[05](./05-security.md) §3.2）。

各オプションの意図:

| オプション | 意図 |
|---|---|
| `-T` | PTY を割り当てない。バイナリが変換されないようにする |
| `Compression=no` | 圧縮しない。音声は 32 KB/秒で、圧縮の利得より遅延を避ける |
| `ControlMaster=no` | 自分がマスターになることはない |
| `ControlPath=none` | 既存の多重化接続に相乗りしない。`--mux` のときだけ外す（§4.3） |
| `ServerAlive*` | 応答の無い接続を検出する |
| `PermitLocalCommand=no` | `LocalCommand` から起動された micmux が、自分の接続でまた起動されるのを防ぐ（§5.3） |
| `BatchMode=yes` | `--background` のときだけ。端末が無いので、尋ねられたら待たずに失敗させる |

### 2.2 `--transport` による差し替え

`--transport <cmd>` を指定すると、`ssh` の代わりに `sh -c <cmd>` を通信路にする。`docker exec` などにも使える。
この場合、micmux は ssh のオプションを付けないので、PTY を割り当てない設定などは利用者の責任になる。

### 2.3 既定では、端末用の接続と TCP を分ける

端末用の SSH と音声用の SSH を別の TCP 接続にするのが既定である。同じ TCP を共有すると、
端末に大量の出力が流れているときに音声が待たされうる。

ただし、接続のたびに指紋認証などが要る鍵では、接続が 2 本だと認証も 2 回になる。その場合は
`--mux` で端末用の接続に相乗りできる（§4.3）。実測では、通常の使い方で両者の遅延に差は無かった
（[04](./04-audio.md) §6）。

## 3. client の内部

### 3.1 録音の条件

```
recording = armed && connected && requested
```

- `armed`: 利用者が録音を許可している。起動直後は偽（`--armed` で真にできる）。
- `connected`: bridge とのハンドシェイクが済んでいる。
- `requested`: bridge から START が届いていて、まだ STOP が来ていない。

armed でないときに START が届いたら、`AudioEnd(DENIED)` を返して断る。
要求が続いているあいだに arm されたら、その時点で録音を始める。

### 3.2 スレッド

| スレッド | 役割 |
|---|---|
| メイン | イベントを 1 本のチャネルで受け、状態を進める。500 ms ごとに起きて、時間上限・監視対象・疎通確認を見る |
| 受信 | bridge からのフレームを読んでメインへ渡す。メインが受け取っていない分は 64 件までに制限する |
| 取得 | 録音セッションごとに 1 本。マイクを開いて PCM を送る。止めるときはスレッドごと終わらせる |
| 操作用ソケット | 接続を受け、1 行のコマンドをメインへ渡して返事を書く |
| 端末の入力 | Enter で arm / disarm を切り替える（端末から動かしているときだけ意味がある） |
| 標準エラーの中継 | ssh と bridge の標準エラーを、制限をかけてから表示する |

- 取得スレッドは、送信用の writer を共有する。**送信が詰まると、取得スレッドの停止が送信の完了待ちになる**
  （gap。[#1](https://github.com/Daiius/micmux/issues/1)）。
- マイクのコールバックの中では、通信もロック待ちもしない。有界のキューに積むだけにして、
  あふれた分は捨てて数を記録する。

### 3.3 疎通確認

client は **10 秒ごとに Ping を送る**。録音中も送る（音声の送信と重なったときは見送る）。
bridge はこれを、接続元が生きているかの判断に使う（§5.2）。往復時間は `micmux status` の `rtt` に出る。

## 4. 起動の仕方

### 4.1 手動

```sh
micmux connect dev-server --set-default
```

端末の前面で動く。Enter で arm / disarm を切り替え、`q` で終わる。

### 4.2 `ssh` に連動して自動で起動する（`LocalCommand`）

```
Host dev-server
    PermitLocalCommand yes
    LocalCommand micmux connect %n --set-default --background --watch-pid $PPID
```

- `--background`: 自分自身を新しいセッションで起動し直し、起動コマンドはすぐに戻る。
  **fork するだけでは済ませない**（決定）。macOS では、fork しただけの子プロセスから Core Audio などの
  システムフレームワークを使うと動作が保証されないため、必ず exec し直す。
- `--watch-pid $PPID`: `$PPID` は `LocalCommand` を実行した ssh のプロセス番号。その ssh が終わると
  micmux も終わる。
- 表示は `~/.local/state/micmux/client.log` に出る（[06](./06-operations.md) §4）。

### 4.3 多重化に相乗りする（`Match exec` + `--mux`）

```
Host dev-server
    ControlMaster auto
    ControlPath ~/.ssh/cm-%C
    ControlPersist 1m

Match originalhost dev-server exec "micmux connect %n --mux --background --watch-pid $PPID --set-default"
```

- `--mux`: `ControlPath=none` を付けず、端末用の ssh が作ったマスターに相乗りする。認証は 1 回で済む。
- **起動のきっかけを `Match exec` にする**（決定）。相乗りした 2 本目以降の ssh では `LocalCommand` が
  走らないため。`Match exec` は ssh を実行するたびに走る。
- **micmux は、端末用の ssh が認証を終えるのを待ってから接続する**。`ssh -O check` が成功するまで
  300 ms ごとに確かめ、120 秒で諦める。待たずに接続すると、micmux の ssh が先に認証を始めて 2 回になる。
- **`ControlMaster`・`ControlPath`・`ControlPersist` の 3 つが必須**。起動時に `ssh -G` で確かめ、
  足りなければ失敗させる。`ControlPersist` が無いと、端末用の ssh はログアウト時に micmux の接続が
  終わるのを待ち、micmux はその ssh が終わるのを待つので、互いに終われなくなる。
- `ControlPersist` の時間は「最後の ssh を閉じてから、マスターを残しておく時間」で、接続中には効かない。
  短くてよい（`0` は無期限の意味になるので使わない）。

### 4.4 複数の ssh と 1 つの micmux

接続元で同時に動く client は 1 つ。同じホストへ ssh を複数開いたときは、次のように振る舞う。

- 2 本目以降の `micmux connect` は、動いている client の操作用ソケットに `watch <pid> <host>` を送り、
  自分は何もせずに終わる（**相乗り**）。
- 動いている client は、登録された pid を監視対象に加える。**監視対象がすべて居なくなったら終わる。**
- 自分が `--watch-pid` を持たずに起動した client（手動起動）は、相乗りを受け付けても終了の条件にしない。
- 接続の確立前（多重化のマスター待ち・ハンドシェイク中）に届いた登録や操作も、捨てずに処理する。
- 監視対象は 64 件まで。1 以下の pid は拒否する。

## 5. 排他と後片付け

### 5.1 接続元: client は 1 つ

- `<操作用ソケットのパス>.lock` に `flock` をかけて、起動を排他する。取れなかった側は、相手のソケットが
  開くのを最大 5 秒待って相乗りする。
- 前回の残骸のソケットを消して作り直すのは、排他を握った側だけ。
- 終了時にソケットを消すのは、自分が作ったもの（デバイス番号と inode が一致）のときだけ。

### 5.2 接続先: 同じ仮想マイクを使う bridge は 1 つ

接続元がスリープしたりネットワークが切り替わったりすると、TCP が半開きのまま残り、bridge には
切断が伝わらないことがある（sshd の既定では何時間も）。古い bridge が残ったまま新しい bridge が
同じ仮想マイクを使い始めると、古い bridge が後で終わるときに仮想マイクを消してしまう。これを 2 つの仕組みで防ぐ。

- **後から来た bridge が、前の bridge を終わらせる**（決定）。排他用のファイルに fcntl のレコードロックを
  かけ、取れなければ持ち主の pid を OS から引いて SIGTERM を送る。3 秒で終わらなければ SIGKILL、
  6 秒で諦めて続ける。pid ファイルと違い、無関係なプロセスを止めることがない。
- **bridge は、30 分間何も届かなければ自分で終わる**。接続元が戻ってこない場合の掃除である。
  **この時間は長めにする**（決定）。短くすると、接続元が数分スリープしただけで bridge が先に終わり、
  端末の SSH は生きているのに音声だけ使えなくなる。

### 5.3 自分で自分を起動しない

`LocalCommand` や `Match exec` から起動している場合、micmux 自身の ssh でもそれらが発動する。

- `LocalCommand` は、`-o PermitLocalCommand=no` で止める。
- `Match exec` は止められないので、micmux が起動する ssh すべてに環境変数 `MICMUX_INTERNAL=1` を付ける。
  そこから起動された `micmux connect` は、これを見て何もせずに終わる。

### 5.4 終わり方

| きっかけ | 接続元 | 接続先 |
|---|---|---|
| `q`・監視対象の終了 | 録音を止め、ssh の標準入力を閉じる。3 秒待って ssh を止める | 標準入力の EOF を見て、後片付けをして終わる |
| client の強制終了・端末ごとの切断 | OS がマイクを閉じる | 同上。SIGHUP は無視し、EOF で気づく |
| bridge への SIGTERM / SIGINT | 接続が切れたとして終わる | 1 秒以内に気づき、後片付けをして終わる |
| 音声サーバーの停止 | Error を受けて終わる | 後片付けをして終わる |

bridge の後片付けは、`pacat` の停止、`pactl subscribe` の停止、既定の録音デバイスの復元、
自分で読み込んだ module の unload である。**ログの出力で落ちない**ようにする（決定）。接続が切れると
標準エラーも閉じるが、そこで panic すると後片付けを通らないので、書き込みの失敗は無視する。

## 6. 未確定・gap

- 自動再接続はしない。接続が切れたら client は終わる。スリープから 30 分以上経って復帰した場合などは、
  ssh を開き直す必要がある（計画中。[07](./07-roadmap.md)）。
- `--ssh-arg` の効き方が、接続本体と多重化の下調べとで食い違っている
  （[#4](https://github.com/Daiius/micmux/issues/4)）。
