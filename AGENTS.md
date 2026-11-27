# AGENTS.md

micmux の開発を手伝うエージェント向けの手引き。

> 仕様の正典は [`prd/`](./prd/)。利用者向けの手順は [`README.md`](./README.md)。

## micmux とは

SSH 接続元のマイクを、接続先の仮想マイクとして使えるようにするツール。接続先のアプリが録音を始めた
ときだけ、接続元のマイクを起動する。単一の Rust バイナリで、接続元では `micmux connect`、
接続先では `micmux bridge` として動く。→ 詳細は [`prd/README.md`](./prd/README.md)。

## ドキュメント（PRD）

| 文書 | 内容 |
|---|---|
| [prd/README.md](./prd/README.md) | 目的 / スコープ / アーキ概観 / 索引 / 公開リポジトリでの秘匿方針 |
| [prd/01-motivation.md](./prd/01-motivation.md) | 動機と要求 / 検討した代替手段 |
| [prd/02-architecture.md](./prd/02-architecture.md) | 構成 / 通信路 / 起動方式 / 排他と後片付け |
| [prd/03-protocol.md](./prd/03-protocol.md) | フレーム形式 / メッセージ / 録音セッションの流れ |
| [prd/04-audio.md](./prd/04-audio.md) | 仮想マイク / 録音要求の検出 / アプリ側の録音経路 / マイク取得 / 実測値 |
| [prd/05-security.md](./prd/05-security.md) | 信頼の方針 / 録音の許可 / 攻撃面ごとの扱い |
| [prd/06-operations.md](./prd/06-operations.md) | セットアップ / 設定 / 操作 / ログと調査 |
| [prd/07-roadmap.md](./prd/07-roadmap.md) | 実装済み / 実機での確認の状況 / 計画中 / 未対応の指摘 |

> 決定の経緯: [`prd/_grilling/decisions.md`](./prd/_grilling/decisions.md)

仕様を変える変更は、同じ変更の中で PRD も直す。判断を覆したときは、決定ログの古い節に
「改定済み」の印を付ける（決定ログの冒頭を参照）。

## ビルドと確認

```sh
cargo build --release
cargo test
cargo clippy --all-targets
```

- 依存 crate は `libc` と `cpal` だけ。増やさない（[prd/02](./prd/02-architecture.md) §1）。
- 接続元の側のツールチェーンの版が決まっていないので、新しい Rust でしか使えない機能を避ける
  （[#7](https://github.com/Daiius/micmux/issues/7)）。
- `cfg(target_os = "macos")` の分岐は、Linux ではビルドできない。触ったら、その旨を報告し、
  macOS の実機で確かめてもらう。
- cpal を使う側のコードは、Linux でも `--features native` でビルドして型を確かめられる
  （`libasound2-dev` が要る）。

## 🔒 動作試験は、隔離した環境で行う

開発に使うマシンが、そのまま接続先として使われていることがある。その場合、利用者の**実運用の音声サーバーと
`micmux bridge` が動いている**。試験でこれらに触れない。

- 🔒 **プロセスを名前で止めない。** `pkill`・`killall`・`pgrep ... | xargs kill` は使わない。
  自分が起動したものを、PID を指定して止める。
  - **なぜ**: 実際に踏んだ（2026-11-27）。試験用に立てたつもりの音声サーバーは起動に失敗していて、
    名前で止めた結果、利用者が使っていた本物の音声サーバーが止まり、動いていた接続も切れた。
- 🔒 **隔離用の環境変数を付けずに、`micmux connect` / `micmux bridge` / `pactl` の変更系を実行しない。**
- `systemctl` で何も止めない・再起動しない。`~/.ssh`・`~/.cargo/bin`・実運用のログやソケットを変更しない
  （`cargo install` は、利用者に頼まれたときだけ）。

### 隔離した音声サーバーの立て方

作業用のディレクトリを `$S` とする（リポジトリの外に置く）。

```sh
export PULSE_RUNTIME_PATH=$S/pulse-rt PULSE_STATE_PATH=$S/pulse-state PULSE_SERVER=unix:$S/pulse-rt/native
export MICMUX_SOCKET=$S/test.sock XDG_STATE_HOME=$S/state XDG_RUNTIME_DIR=$S/rt
mkdir -p $S/rt $S/pulse-rt
DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent pulseaudio -n \
    -L "module-native-protocol-unix socket=$S/pulse-rt/native" \
    --daemonize=no --exit-idle-time=-1 --use-pid-file=no & PA=$!
```

- D-Bus を切り離さないと、実運用の音声サーバーと名前が衝突して起動しない。
- 起動したら、`pactl info` の `Server String` が `$S` の下を指していることを確かめてから使う。
- 終わったら `kill $PA` で止める。

### client と bridge を 1 台の中でつなぐ

```sh
micmux connect --transport "/path/to/micmux bridge --sink-name mmtest" --capture-cmd 'cat /dev/zero'
```

- `--transport` で、ssh の代わりに bridge を直接起動する。
- `--capture-cmd` で、マイクの代わりのコマンドを指定する。`cat /dev/zero` は実時間より速く流れるので、
  速さが意味を持つ試験には使わない（[#6](https://github.com/Daiius/micmux/issues/6)）。
- ssh の役は、`sleep 300 & P=$!` と `--watch-pid $P` で代用できる。
- SSH そのもの（多重化・`LocalCommand`・`Match exec`）を試すには、`localhost` の高いポートで検証用の
  sshd を立てる。鍵と設定は作業用のディレクトリに置き、利用者の `~/.ssh` には触れない。
  `ControlPath` は長すぎると失敗するので、短い場所にする。

## 公開リポジトリであること

コード・文書・コミットメッセージ・issue に、秘密情報や、利用者の環境を特定する情報（ホスト名・
ユーザー名・IP アドレス・実際のパス）を書かない。`micmux debug` の出力は、これらを含む
（[prd/README.md](./prd/README.md)「公開リポジトリでの秘匿方針」）。

## コミット

- コミットメッセージは 3 行程度に収める。背景や設計の判断は PRD に、確かめ方は PR の本文に書く。
- ステージするときは、パスを明示する。作業ディレクトリにあるものをまとめて拾わない。
