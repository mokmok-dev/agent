# Research Index

外部プロジェクト・設計ドキュメントからの文献調査メモを集める場所。

| 文書 | 対象 | 概要 |
|---|---|---|
| [opencode の SQLite とセッションストレージの役割分担](opencode-sqlite-session-store.md) | anomalyco/opencode (インストール版 v1.18.22) + 本リポジトリ agentd | 共有の永続層(セッション横断)とセッション固有データ(JSON ファイル)の分担。<br>セッションごとの SQLite という前提は opencode には当てはまらない。agentd 側の共有層は JSONL イベントログ。 |
| [codex CLI の sandbox 実装](codex-sandboxing.md) | openai/codex `main` (`codex-rs/sandboxing` ほか関連クレート) | argv をプラットフォーム固有の隔離ラッパーで包んで spawn する方式。二軸の `PermissionProfile`、macOS Seatbelt / Linux bwrap+Landlock+seccomp / Windows restricted token+MXC、拒否検知と violation イベント。 |
| [ZeroClaw の security 設計と sandbox アプローチ](zeroclaw-security-sandbox.md) | zeroclaw-labs/zeroclaw `master` (`crates/zeroclaw-runtime`, `zeroclaw-config`, `zeroclaw-tls`, `zeroclaw-plugins` ほか) | アプリ層ポリシー(autonomy / path / command / approval)と OS sandbox(Landlock / Bubblewrap / Firejail / Seatbelt / Docker)の多層防御。pairing/OTP/WebAuthn/mTLS、ハッシュチェーン監査、tool receipts、WASM plugin、サプライチェーン。 |