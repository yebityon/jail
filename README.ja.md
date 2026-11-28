# jail

Claude Code・Codex CLIを、プロジェクトごとの[Apple container](https://github.com/apple/container)で実行するターミナルツール。

[English](README.md) · 日本語

> **Toyプロジェクト／実験的なプロトタイプです。** 個人の実験・学習用で、本番利用を想定していません。第三者によるセキュリティ監査は受けておらず、本番環境のセキュリティ境界として信頼しないでください。破棄できるプロジェクト・アカウントで試し、機密データや本番用の認証情報は避けてください。動作やCLIの互換性は予告なく変わる場合があります。

Apple・Anthropic・OpenAIによる公式提供・推奨ではない、独立したコミュニティプロジェクトです。

```sh
cd /path/to/your/project
jail claude
jail codex
```

`jail`はRust製のCLIです。プロジェクトごとに再利用できるLinuxコンテナを管理し、プロジェクトをホストと同じパスで共有します。作業ディレクトリとCLIの引数も引き継ぎます。

> 既定では共有プロジェクトへの書き込みと、制限のない外向き通信を許可します。機密性の高いコードを扱う場合やホストの認証情報を取り込む場合は、先に設定を確認してください。

## 特徴

- **プロジェクト単位の環境** — コンテナを再利用し、専用の永続ホームvolumeを保持。
- **ログイン情報を隠して再利用** — サブスクリプションOAuthはroot専用ゲートウェイに保持し、エージェントにはダミーだけを渡す。APIキーは不要。
- **書き込み保護** — Gitの設定・hooksやエージェント設定は読み取り専用。列挙したパスだけを書き込み可能にすることも可能。
- **調整可能な通信ポリシー** — open・allowlist・noneから選択。許可リストは再起動なしで変更可能。
- **開発サーバーに接続** — Macの`127.0.0.1`へTCPポートを転送。
- **使い捨てセッション** — ホストを一切マウントしないコンテナで起動し、終了時に削除。

## インストール

Apple Silicon搭載Mac（macOS 26以降）、Apple `container` 1.5系、Rust 1.85以降が必要です。チェックアウトしたディレクトリのルートで実行します。

```sh
brew install container
cargo install --path . --bin jail --locked
jail config init   # ~/.config/jail/config.toml を作成
```

初回起動時に、設定したバージョンのClaude Code・Codexと、Git・Node.js・curl・ripgrepを含むLinuxイメージを構築します。

## 使い方

**jailのオプションはツール名の前**に指定します。`claude`・`codex`以降の引数はそのCLIへ渡します。

```sh
jail claude --resume
jail codex exec "テストを実行して"
jail --auto-stop claude        # 最後のセッション終了時にコンテナを停止
jail --ephemeral claude        # 何もマウントしない使い捨てコンテナ
jail --dry-run claude          # 起動計画だけを表示
```

| コマンド | 用途 |
| --- | --- |
| `jail claude` / `jail codex [args…]` | プロジェクトのコンテナ内でCLIを起動 |
| `jail stop [--all]` | 使用中のセッションがないコンテナを停止。`--force`で現在のコンテナを強制停止 |
| `jail recreate` | 現在のコンテナを削除し、次回起動で新規作成 |
| `jail auth claude` / `jail auth codex` | ホストログインを再取り込み |
| `jail policy show` / `log` / `allow HOST` / `reload` | 通信の許可リストを確認・変更 |
| `jail config show` | 解決済みの設定を表示 |
| `jail cleanup [volume NAME… \| image TAG… \| --all-delete]` | jailのホームvolume・イメージを一覧表示・削除 |

コンテナは起動のたびに再利用します。設定やイメージが変わった場合は、セッションを閉じて`jail recreate`を実行してください。ホームvolume・CLIの履歴・ホストのファイルは残ります。ログインは**Macで**`claude auth login`または`codex login`を実行します。実行中のセッションには約15秒以内に反映されます。

コンテナのライフサイクル、削除、使い捨てセッション、開発サーバー、トラブルシューティングは[docs/usage.ja.md](docs/usage.ja.md)を参照してください。

## 設定

設定は信頼できる入力として、`~/.config/jail/config.toml`（または`--config`・`JAIL_CONFIG`）から読み込みます。プロジェクト内の設定は読み込みません。全項目は[config.example.toml](config.example.toml)にあります。例：

```toml
[workspace]
allow_write = ["src/", "tests/"]   # ワークスペースのそれ以外は読み取り専用
hidden = [".env", "secrets"]

[network]
mode = "allowlist"
allowed_hosts = ["api.anthropic.com", "claude.ai", "platform.claude.com",
                 "chatgpt.com", "auth.openai.com", "api.openai.com"]
```

書き込み保護、通信ポリシー、CLIのツール制限は[docs/configuration.ja.md](docs/configuration.ja.md)を参照してください。

## セキュリティ

jailはLinux実行環境を分離しますが、**信頼できないエージェントやプロジェクトを既定で安全にするものではありません**。エージェントは非root・capabilityなしで動きます。ホームディレクトリ・SSH agent・containerのソケットは共有せず、実際のログイントークンはエージェントから隠します。共有ファイルと許可された推論は、設定した権限の範囲で利用できます。

認証、隔離の境界、制限事項は[docs/security.ja.md](docs/security.ja.md)を参照してください。

## 開発

```sh
cargo fmt --all --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

バグ報告・プルリクエストを歓迎します。認証情報やトークンは添付しないでください。テスト、ファジング、smoke test、コードの構成は[docs/development.ja.md](docs/development.ja.md)を参照してください。

## ライセンス

[MIT](LICENSE) © 2026 jail contributors。対象はjail自身のソースコードです。イメージ構築時にダウンロードするCLIなど、第三者のツールにはそれぞれのライセンスが適用されます。
