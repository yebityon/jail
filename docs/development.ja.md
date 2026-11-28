# 開発とコントリビューション

[English](development.md) · 日本語 · [README](../README.ja.md)

バグ報告、ドキュメントの改善、プルリクエストを歓迎します。macOS・container・jail・エージェントCLIのバージョン、最小限の再現手順、機密情報を除いた設定を添えてください。認証情報・トークン・未加工のコマンド引数は添付しないでください。

リポジトリのルートから実行します。

```sh
cargo fmt --all --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release --bin jail
./target/release/jail --dry-run claude --resume
```

統合テストにはPython 3が必要です。macOS 27・Apple `container` 1.5.0で実機検証しています。

統合テストはPython製のApple container代替と一時的なloopbackリスナーを使います。実際のcontainerサービスには接続せず、ホスト認証も取り込みません。

CIはmacOSで上記を実行します。arm64 Linuxでは、ライブラリと`jail-guest`監督プロセスのClippyとテストを実行します。`cfg(target_os = "linux")`のコードは、Linuxでだけコンパイルされます。[rust-toolchain.toml](../rust-toolchain.toml)は、`container/Dockerfile`が`jail-guest`のビルドに使うRustのリリースに固定します。

エージェントからの入力を読むパーサーのファズターゲットは[fuzz](../fuzz)にあります。nightlyのRustと[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz)が必要です。CIでは実行しません。

```sh
cargo +nightly fuzz run gateway_request   # proxy_request、base64も同様
```

releaseバイナリをビルドした後、必要に応じて実コンテナで検証できます。

```sh
./scripts/smoke-test.sh
./scripts/auth-smoke-test.sh
# localhostの模擬サーバーだけでCLIのHTTP/SSEを確認（macOS sandbox-exec必須）:
python3 scripts/protocol-smoke-test.py
```

コンテナのテストは専用の一時コンテナとホームvolumeを作成・削除し、既存プロジェクトには触れません。smoke testはホスト認証なしで保護パスの置換拒否・通信拒否ログ／live reload・各制限・転送・Codexサンドボックスを確認します。auth testはホストログインをroot専用tmpfsへ取り込み、認証認識・トークン非公開・モデル一覧の読み取りを確認します。推論は実行しません。protocol testはダミー認証とlocalhostの模擬SSEサーバーだけを使い、macOS sandboxで外向き通信を遮断します。

ホスト側のエントリーポイントは[src/host/main.rs](../src/host/main.rs)、ランタイム管理は[src/host/runtime.rs](../src/host/runtime.rs)です。Appleの`container`コマンドの呼び出しは、すべて[src/host/apple_container](../src/host/apple_container)を通します。Linux側のスーパーバイザーは[src/container/main.rs](../src/container/main.rs)です。両側が使うコードは[src/shared](../src/shared)にあります。

`JAIL_HOME`でホストの管理状態・ビルド用ディレクトリ（通常は`~/.local/share/jail`）を変更できます。`JAIL_CONTAINER_BIN`はテスト用のbackend実行ファイルを指定します。変更した動作のテストを追加し、英語・日本語のREADMEとdocsを合わせて更新してください。予定する作業は[TODO.md](../TODO.md)で管理しています。
