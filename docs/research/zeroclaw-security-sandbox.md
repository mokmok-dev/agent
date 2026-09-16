# ZeroClaw の security 設計と sandbox アプローチ

> 出典調査: [zeroclaw-labs/zeroclaw](https://github.com/zeroclaw-labs/zeroclaw) を `--depth 1` で clone し、master 先頭コミット `d8d5d93 feat(dist): add canonical release target registry (#10590)` 時点のソース・ドキュメントを読んだメモ。
>
> 検証方法: ローカル clone の `crates/`・`src/`・`docs/book/src/`・`.github/workflows/` を一次情報として直接読んだ。扱う範囲が広いため、OS sandbox 実装、ポリシー/ツール強制、identity/監査、docs/plugin/サプライチェーンの4領域を並列に調査して統合している。本メモでは全文転記を避け、型・定数・既定値と `file:line` を中心に記す。特定コミットには固定していない(取得時点の master)。

## 結論

**ZeroClaw の防御は「アプリケーション層のポリシー(autonomy / path / command / approval)」と「OS ネイティブ sandbox」の多層防御(defense-in-depth)である。** 既定は `supervised`(低リスクは実行、中リスクは承認要求、高リスクは遮断)で、ファイル操作は workspace 配下と許可 root に閉じ、シェルは allowlist + 構文検査で縛り、OS sandbox が最終防衛線として filesystem/network を制限する。codex の sandbox とは対照的に、**ポリシー判定はアプリケーション側に厚く、OS sandbox はバックエンド選択制(かつ既定ビルドでは必ずしも有効でない)** という構造になっている。

| 軸 | 実装 |
|---|---|
| ポリシー表現 | `AutonomyLevel`(`ReadOnly`/`Supervised`/`Full`) × `RiskProfileConfig`(workspace_only, allowed_roots, allowed_commands, forbidden_paths, rate/cost caps) |
| ツール前段ゲート | `ApprovalManager::approval_requirement`(Prompt/Approved/NotRequired) + 各ツール内の事前検証 |
| コマンド | allowlist 方式(`allowed_commands`、既定 20+ コマンド)。denylist は存在しない。構文検査(`is_posix_like_command_allowed`)で `$()`・backtick・危険 redirect 等を拒否。`CommandRiskLevel`(Low/Medium/High) |
| パス | `workspace_only=true` 既定 + `forbidden_paths`(既定 `/etc,/root,/home,/usr,... ~/.ssh ~/.gnupg ~/.aws`) + `..`・URL エンコード traversal 拒否 + symlink 解決(上限 64 hop、fail-closed)。most-specific-match / deny-wins |
| macOS sandbox | `/usr/bin/sandbox-exec` + 生成 SBPL(`(deny default)`、workspace read/write、localhost のみ outbound) |
| Linux sandbox | Landlock(feature `sandbox-landlock`、filesystem のみ)/ Bubblewrap(`--unshare-all`)/ Firejail。既定ビルドでは Landlock 非同梱 |
| Docker | `DockerSandbox`(`--network none`, workspace ro)と `DockerRuntime`(`runtime.kind=docker` で container 自体が境界)の2系統。nested sandbox は自動回避 |
| 監査 | JSON Lines + SHA-256 ハッシュチェーン(genesis 64 zero、`prev_hash` 連鎖)、任意で HMAC 署名。`verify_chain` で検証 |
| 認証 | pairing(`zc_<hex>` トークン、SHA-256 保存)、OTP(TOTP)、WebAuthn/passkey、mTLS(TLS1.3 + client cert + pin + CRL)、OIDC/Nevis |
| provider 信頼 | tool receipts(HMAC-SHA256、ephemeral key)、LeakDetector(秘密情報検出)、PromptGuard(注入検知)、emergency stop(OTP で解除) |
| plugin | WASM Component Model(wasmtime)、deny-by-default capability、fuel/time/memory 制限。ただし一部 permission は未配線 |

## 全体像: 6 層モデル

ZeroClaw は自身の防御を6層で説明している(`docs/book/src/security/model.md:3-67`)。

1. **channel の pairing / アクセス制御** — メッセージが runtime に入る前に `allowed_users` / `allowed_chats` / IP allowlist / pairing で制限。
2. **autonomy level** — `readonly` / `supervised` / `full`。
3. **workspace 境界とパス規則** — 既定 forbidden paths(`/etc`, `/sys`, `/boot`, `~/.ssh` 等)。
4. **shell command policy** — `allowed_commands` と `validate_command_execution` の構文パス。
5. **OS-level sandbox** — Landlock / Bubblewrap / Firejail / Seatbelt / Docker。
6. **tool receipts** — HMAC によるツール実行の改ざん/捏造検知。

加えて、OTP ゲート、emergency stop、prompt-injection guard、leak detector、pairing guard が層として挙げられている(`model.md:69-77`)。

哲学は `docs/book/src/philosophy/security-first.md:3-14` に集約されており、「shell を実行し HTTP を叩きファイルを書ける agent は privileged process である」「既定は supervised」「YOLO は意図的な避難口であり既定ではない」としている。ただし **ランタイム全体を覆う単一の正式な threat model 文書は存在しない**(receipts と SLSA に個別の threat model があるのみ)。

## Autonomy とリスクモデル

`AutonomyLevel`(`crates/zeroclaw-config/src/autonomy.rs:19-27`、既定 `Supervised`):

| level | 意味 | 実装上の効果 |
|---|---|---|
| `ReadOnly` | 観測のみ。shell 全拒否、file_write 拒否 | `can_act` false(policy.rs:4279-4281)、全コマンド拒否(policy.rs:3382,3444)、write は "autonomy is read-only"(file_write.rs:98-104) |
| `Supervised`(既定) | 低リスク実行・中リスク承認・高リスク遮断 | `approval_requirement` が Prompt を返す(approval/mod.rs:209-210) |
| `Full` | 承認ゲートなし。**`workspace_only` を暗黙的に false に落とす** | approval は Approved 固定(approval/mod.rs:176-179)、`from_profiles` が `effective_workspace_only=false`(policy.rs:4503-4510)。forbidden_paths と OS sandbox は残る |

`RiskProfileConfig::level` / `workspace_only` の既定は `Supervised` / `true`(`schema.rs:13592-13593`)。リスクプロファイルは `[risk_profiles.<alias>]` として定義し、`agents.<alias>.risk_profile` から参照する。

コマンド単位のリスクは `CommandRiskLevel`(Low/Medium/High)で、`command_risk_level_for_shell` がシェル区切りで文を分割し**最も高いリスクを採用**する(policy.rs:3062-3203)。PowerShell の未知 verb は High に強制される(policy.rs:3189-3194)。

## アプリケーション層の強制

### コマンド実行ポリシー(`crates/zeroclaw-config/src/policy.rs`)

- **allowlist 方式**。`allowed_commands` の既定は git, npm, cargo, ls, cat, grep, find, echo, pwd, wc, head, tail, date, df, du, uname, uptime, hostname, python/python3, pip, node など(policy.rs:407-437)。**`forbidden_commands` という denylist は実在しない**(system prompt には書かれているが config field も実行時チェックもない)。
- 構文検査 `is_posix_like_command_allowed`(policy.rs:3443-3537): backtick、未クォート `$VAR`、process substitution `<(...)`、危険な file redirect、`tee`、単独 `&`(background)を拒否。`2>/dev/null`、`2>&1`、heredoc は許可。
- 引数の穴 `is_args_safe`(policy.rs:3540-3602): `find -exec`、`git -c/alias/config`、`env`、`python -c/-m`、`node -e`、`pip install`、`npm exec/install`、`cargo install` を制限。
- 破壊的パターン(`generic_segment_risk`, policy.rs:2960-3060): `rm, mkfs, dd, shutdown, sudo, chown, mount, iptables, curl, wget, nc, ssh` 等を High とし、`rm -rf /`、fork bomb、`format c:` 等の literal も検出。
- `block_high_risk_commands` が true なら High は拒否、Supervised で `!approved` なら承認要求(policy.rs:3253-3275)。
- **逃げ道**: `allowed_commands` に `"*"` があり `block_high_risk_commands=false` なら、構文制約をすべてスキップして実行を許可する(policy.rs:3452-3455)。YOLO 相当。

### パス/workspace ポリシー

- `workspace_only=true` 既定。`allowed_roots`(+ read-only / write-only の3階層)で追加許可。
- `default_forbidden_paths()`(policy.rs:480-501): `/etc,/root,/home,/usr,/bin,/sbin,/lib,/opt,/boot,/dev,/proc,/sys,/var,/tmp, ~/.ssh, ~/.gnupg, ~/.aws, ~/.config`。Windows 版も別途定義。
- `is_path_allowed`(policy.rs:3943-4036): NUL byte 拒否、`..` コンポーネント拒否、URL エンコード `..%2f` 拒否、`~user` 拒否、`/dev/null` は常時許可、絶対パスは workspace/roots 内なら通過(ただし forbidden が上書き)。
- symlink 解決 `resolve_symlinked_path`(policy.rs:816-886): 最深既存 prefix を canonicalize、dangling symlink も追う。**上限 64 hop**、超過時は fail-closed(policy.rs:836)。
- 優先順位 `deepest_allow_depth` / `deepest_forbidden_depth` を深さで比較し、**同深度は deny が勝つ**(`forbidden_overrides_allow`, policy.rs:629-635)。lexical と resolved を混同しないため `PathMatchNamespace` で名前空間を分ける(policy.rs:554-575)。
- コマンドライン内のパス引数走査 `forbidden_workspace_path_argument*`(policy.rs:3611-3835)は best-effort であり、`$VAR`・`eval`・スクリプト内部・空白入りクォートパスは見えないと明記されている(policy.rs:3844-3860)。

### レート制限・コスト上限

- `ACTION_WINDOW = 3600s`、`max_actions_per_hour` 既定 **20**(policy.rs:40,768)。`PerSenderTracker` が会話スレッド単位で予約/コミットし、cron/CLI は `__global__` キー(policy.rs:165-277)。`RateLimitedTool` が成功時に課金(wrappers.rs:63-82)。**読み取りツールも Act 予算を消費する**(コメントの「Read は無料」と矛盾)。
- コストは2系統に分断されている: `SecurityPolicy.max_cost_per_day_cents`(既定 500)は**escalation 用で実強制されない**(delegate.rs:650-656)。実際の上限は `[cost] daily_limit_usd`(既定 10.0)を `CostTracker` が判定し、既定 mode は `"warn"`(schema.rs:6852-6963)。

### ファイル読み書きツール

- `file_read`(`crates/zeroclaw-runtime/src/tools/file_read.rs`): `MAX_FILE_SIZE_BYTES = 10 MiB`、canonicalize 後に `is_resolved_path_readable`、サイズ検査も canonicalize 後(TOCTOU 対策)。utf8 / base64。
- `file_write`(`crates/zeroclaw-tools/src/file_write.rs`): read-only 拒否、`is_resolved_path_allowed`、**既定で上書き**(O_EXCL なし)、symlink leaf への書き込み拒否、runtime config(`config.toml`, `estop-state.json`, `otp-secret`, `webauthn_credentials.json` 等)への書き込み拒否(policy.rs:4236-4262)。
- `allowed_tools` は `None`=無制限、`Some([])`=全許可、deny-all は `deny_all_tools=true`/sentinel `__none__`(schema.rs:13537-13543,13125)。空リストが無制限を意味する点に注意。

## Approval フロー

`ApprovalManager::approval_requirement`(crates/zeroclaw-runtime/src/approval/mod.rs:175-211)の順序:

1. `Full` → `Approved`
2. `ReadOnly` → `NotRequired`
3. `always_ask` に該当 → `Prompt`(最優先)
4. 非対話で承認経路なし → `NotRequired`
5. `auto_approve` に該当 → `Approved`
6. セッション allowlist(`Always` で追加) → `Approved`
7. 既定 → `Prompt`

- 既定 `auto_approve` は file_read, memory_recall, web_search, web_fetch, calculator, glob/content search, browser 等(schema.rs:13101-13116)。
- 対話時は `/dev/tty` に確認。非対話時は channel 経由で確認し、経路が無い/エラー時は**自動拒否**(approval_gate.rs:81-118)。
- **fail-closed**: `OnNoApprover::Deny` 既定、`timeout_secs=120`、タイムアウトは拒否扱い(autonomy.rs:68-96)。
- モデルが `approved` 引数を偽装できないよう、runtime が渡す前に `approved` を剥がす(`set_runtime_approved_arg`, agent/mod.rs:44-53)。`approved=true` は承認成立後のみ再注入(call_prep.rs:292-319)。

## OS-level sandbox

### バックエンドと選択

trait は `crates/zeroclaw-runtime/src/security/traits.rs:7-31` の `Sandbox { wrap_command, is_available, name, description, coding_cli_unsupported_reason }`。

| backend | 実装 | 機構 |
|---|---|---|
| `LandlockSandbox` | security/landlock.rs | Linux LSM。`pre_exec` 内で `restrict_self`。**argv は書き換えない** |
| `BubblewrapSandbox` | security/bubblewrap.rs | `bwrap` で argv をラップ、`--unshare-all` |
| `FirejailSandbox` | security/firejail.rs | `firejail --private=home ...` |
| `SeatbeltSandbox` | security/seatbelt.rs | `/usr/bin/sandbox-exec -f <sbpl>` |
| `DockerSandbox` | security/docker.rs | `docker run --network none` でラップ |
| `DockerRuntime` | crates/zeroclaw-config/src/platform/docker.rs | `runtime.kind=docker`、container 自体が境界 |
| `WasmRuntime` | platform/wasm.rs | インプロセスの wasmi isolation |
| `NoopSandbox` | traits.rs:33-53 | 何もしない(既定の `ShellTool::new` はこれ) |
| Windows AppContainer | — | **docs のみ。実装なし** |

選択は `security/detect.rs`。`SelectedSandboxBackend` は None/Landlock/Firejail/Bubblewrap/Docker/DockerRuntime/SandboxExec(detect.rs:89-115)。自動検出順は **Linux: Landlock → Firejail**、**macOS: Bubblewrap → sandbox-exec**、その後 Docker、最終フォールバック `None`(detect.rs:208-247)。Docker は `runtime.kind == Native` のときスキップされる。

> **docs との食い違い**: `sandboxing.md:33-34` は Linux を `Landlock → Bubblewrap → Firejail → Docker`、macOS を `Seatbelt → Docker` と説明するが、コードは Linux で Bubblewrap を自動選択せず、macOS では Bubblewrap を Seatbelt より優先する。Bubblewrap は Linux では明示指定 `sandbox_backend="bubblewrap"` でのみ到達可能。

### 各バックエンドの強制内容

**Landlock**(`landlock.rs`)
- `LandlockSandbox { workspace_dir, allowed_roots, allowed_roots_read_only, allowed_roots_write_only }`(`landlock.rs:20-31`)。`AccessFs` の read/write/execute を bitflags で制御。
- 汎用 allow ルール 23 件(`generic_rules`, landlock.rs:104-237): `/tmp`(Execute なし)、`/usr /bin /lib /lib64`、`/etc/ld.so.*`、`/dev/null`、DNS 設定、TLS 証明書ストア。秘密鍵ディレクトリ(`/etc/ssl` 等)は意図的に除外。
- ruleset 構築(`build_ruleset`, landlock.rs:339-567): workspace を `PathBeneath(read_write)`(開けなければ fail-closed)、追加 root を3階層で追加、**Landlock は rights を加算するだけで減算できない**ため、generic rule とネストする制限 tier は WARN 付きでスキップ(landlock.rs:412-449)。
- 強制は `cmd.pre_exec` 内の `restrict_self`(内部で `PR_SET_NO_NEW_PRIVS` + `landlock_restrict_self`)。**親 daemon は制限されない**(landlock.rs:562-566)。失敗は spawn の Err として伝播し fail-closed。
- **network 制御は一切なし**(filesystem のみ)。
- feature `sandbox-landlock` は既定 feature に含まれない(runtime crate の default は `observability-prometheus` と `schema-export`)。**既定ビルドでは Landlock がコンパイルされない**。

**Bubblewrap**(`bubblewrap.rs:99-145`): `--ro-bind /usr ... --dev /dev --proc /proc --bind /tmp /tmp --unshare-all --die-with-parent` + `CAP_SYS_ADMIN/CAP_SYS_PTRACE` の drop。`--unshare-all` に network が含まれる。`--seccomp` は BPF fd が必要なため使わない。workspace を bind せず cwd も設定しないため、coding CLI では**使用不可**と明示(`coding_cli_unsupported_reason`)。

**Seatbelt**(`seatbelt.rs:83-220`): `/usr/bin/sandbox-exec -f <policy>`、SBPL は `(deny default)` から始まり、`process-exec/fork`、`signal (target self)`、`file-read*`(システムパス + workspace + `/tmp` + user dotfiles)、`file-write*`(workspace + `/tmp` + `/dev/null` + `/dev/tty`)、**network は localhost outbound と mDNSResponder socket のみ**を許可。ポリシーファイルは temp に生成し `Drop` で削除。

**Firejail**(`firejail.rs:111-149`): `--private=home --private-dev --nosound --no3d ... --noprofile --quiet` + `--help` 検出で `--seccomp --caps.drop=all --noroot`。**`--net=none` は付かないため network は既定で遮断されない**。

**DockerSandbox**(`docker.rs:89-122`): `docker run --rm --memory 512m --cpus 1.0 --network none`、workspace を **read-only** mount。image 既定 `alpine:latest`。coding CLI では使用不可。

**DockerRuntime**(`platform/docker.rs:85-142`): `docker run --rm --init --interactive --network <config.network>(既定 none)--read-only(任意)--volume <ws>:/workspace:rw`、command は container 内で `sh -c`。workspace は canonicalize し `allowed_workspace_roots` で検証、`/` は拒否。

### spawn 経路と環境

- 本番配線は `runtime_shell_assembly`(`tools/mod.rs:683-706`)で `Arc<dyn Sandbox>` を1つ作り `ShellTool::new_with_sandbox` に渡す。
- `ShellTool::execute`(shell.rs:250-389): コマンド検証 → workspace パス走査 → `runtime.build_shell_command` → `sandbox.wrap_command`(失敗時は "Sandbox error" で hard fail)→ **`env_clear()`** 後に `SAFE_SHELL_ENV_VARS`(`PATH HOME TERM LANG LC_ALL LC_CTYPE USER SHELL TMPDIR`)∪ 明示 passthrough のみ再設定 → `ZEROCLAW_SESSION_ID` 注入 → `process_group(0)` / `kill_on_drop(true)` / stdin null で spawn。**PTY は使わない**。
- `arg0` 自己 exec のようなトリックは無い。argv を書き換えるのは Bubblewrap/Firejail/Seatbelt/DockerSandbox のみ、Landlock は `pre_exec` のみ。

### fallback / nested sandbox

- backend が `None`、`enabled=Some(false)`、明示 backend が利用不可、自動検出で何も見つからない場合は **`NoopSandbox`** に落ちる(detect.rs:349-398)。`create_selected_sandbox` は構築エラーを `.ok()` で握り潰して None にする(detect.rs:401-486)。
- nested sandbox 回避: runtime が Docker の場合、明示 `Docker` は `DockerRuntime` に読み替え、自動検出でも Docker をスキップする。`docker_runtime_no_nested_sandbox_9231.rs` が「`docker run` が1回だけ、`alpine:latest` が現れない」ことを検証(detect.rs:520-528)。
- `SandboxPosture`(detect.rs:28-87)が「要求 backend と実際の backend の差異」から fallback を報告。

## Identity / 認証 / 信頼

- **pairing**: 32 byte 乱数 → `zc_<hex>` トークン。保存は SHA-256 ハッシュ、定数時間比較。pairing code は TTL 10 分、試行5回でロックアウト(crates/zeroclaw-config/src/pairing.rs)。
- **gateway auth**: `Authorization: Bearer`(`api.rs:32-68`)、WS は `Sec-WebSocket-Protocol: bearer.<token>` と `?token=` も許容(ws.rs:66-141)。レート制限 10 req/60s、ロックアウト 300s(loopback 免除)。
- **principal**: `PrincipalId` / `ActorKind` / `AuthMethod` / `IdentitySubject`(SharedOperator/Roster/Oidc/Service)を `crates/zeroclaw-api/src/principal.rs` で定義。`principal_resolver.rs` が provider 検証済み identity を権限(`ResolvedGrants`)へ写像し、ポリシー世代(generation)で再解決。OIDC は issuer 一致を検証、service は `service_profile_map` 経由のみ。**`AuthProvider` レジストリは default-deny だが本番未配線**(auth_provider.rs:24-28)。
- **OTP**: TOTP(RFC 6238、6桁、HMAC-SHA1、replay cache)。SecretStore で暗号化し 0600 保存。既定 disabled、shell/file_write/browser/memory_forget がゲート対象(security/otp.rs)。
- **WebAuthn**: ES256/P-256、challenge 32 byte、origin/rpIdHash/UP 検証、sign-counter による clone 検知。credential は SecretStore 暗号化、既定 disabled(security/webauthn.rs)。
- **Nevis**: Keycloak 互換 IAM。remote introspection は実装、**local JWKS 検証は未実装で bail**(nevis.rs:233-245)。`IamPolicy` は role→tool/workspace を deny-by-default で写像。
- **mTLS / PKI**: `zeroclaw-tls` は rustls。TLS1.3 のみの mTLS acceptor、`WebPkiClientVerifier` → `PinnedCertVerifier`(SHA-256 fingerprint pin)→ `RevocationCheckVerifier`(CRL、失敗時 fail-closed)。ECDSA P-256、client cert 有効期限 30 日。CA 鍵は scrypt + XChaCha20-Poly1305 で保護可能。`cert_ledger.rs` が発行済み client 証明書を SQLite で台帳管理し、issuance は audit と2相コミット、undelivered は削除せず revoke。
- **verifiable intent**: SD-JWT/JWS チェーン(ES256)、L1 issuer→user / L2 user→agent / L3 payment・checkout。`cnf.jwk.kid` による鍵束縛、`sd_hash`・`checkout_hash`・timestamp(300s skew)検証。ただし **chain verifier が未構築**で `VerifiedCredentialChain` を生成できない(verification.rs:27-79)。

## 監査・receipts・フォレンジック

- **audit**(`security/audit.rs`): JSON Lines、`AuditEvent` は timestamp/event_id/type/actor/action/result/security_context + `sequence`/`prev_hash`/`entry_hash`/`signature`。`entry_hash = SHA-256(prev_hash || canonical_json)`、genesis は 64 zero の **ハッシュチェーン**(Merkle ではない)。`sign_events=true` で `ZEROCLAW_AUDIT_SIGNING_KEY` による HMAC 署名。fsync 付き append、`.1.log`..`.10.log` ローテーション、`verify_chain` で検証。既定 enabled=true / sign=false。
- **tool receipts**(`agent/tool_receipts.rs`): HMAC-SHA256(ephemeral in-memory key、ring SystemRandom)、`tool_name|args|result|timestamp`、`zc-receipt-{ts}-{b64}`。ツール結果に付けモデルへ返し、捏造/否認を検知。**非 ZK・第三者検証不可・scope 跨ぎ不可・永続 DB は Planned**。既定 disabled。leak detector は `zc-receipt-*` を whitelist。
- **emergency stop**(`security/estop.rs`): `KillAll`/`NetworkKill`/`DomainBlock`/`ToolFreeze`。状態は `estop-state.json` に atomic 保存、壊れていたら kill_all に fail-closed。解除に OTP 必須(既定 true)。
- **prompt injection**: `prompt_guard.rs`(重み付き6カテゴリ、system override/role confusion/secret extraction/jailbreak/tool_call 注入/shell metachar、既定 Warn・sensitivity 0.7)と `external_content.rs`(zero-width 除去、全角正規化、fence marker 無効化、8192 byte cap、`<<<EXTERNAL_UNTRUSTED_CONTENT id=...>>>` で囲む)。**`ingress.rs` は常に Loop を返す stub**。
- **leak detector**: Stripe/OpenAI/Anthropic/Groq/Google/GitHub/Slack の API key、AWS、PEM 秘密鍵、JWT、DB URL、bot token、高エントロピートークンを検出し `[REDACTED_*]` へ置換。`SecretStore` は ChaCha20-Poly1305(`enc2:` 形式)。
- **vulnerability / playbook**: Nessus/Qualys JSON の取り込み、CVSS + internet-facing/production 重み付け、playbook の auto-approve 制御(未知 severity は `u8::MAX` で自動承認不可)。

## Plugin / サプライチェーン / コンテナ

- **plugin**: wasmtime + WASM Component Model(`wit/v0`、`tool-plugin`/`channel-plugin`/`memory-plugin`)。**既定 disabled・deny-by-default**、`PluginPermission`(HttpClient/FileRead/FileWrite/Memory/ConfigRead 等)のうち実際に効くのは `config_read` と `http_client` のみ。**no ambient authority**(filesystem preopen なし・network なし)。fuel(既定 1e9)・wall-clock(30s)・memory(256MB)・table/instance 制限。署名は Ed25519 だが **`.wasm` 本体は署名対象外**(manifest のみ)。`[egress]` の host-owned egress authority は ADR-014 で **proposed**(未実装)。
- **サプライチェーン**: SLSA v1.0 Build L2 の GitHub artifact attestations(`actions/attest`)、SPDX/CycloneDX SBOM、GHCR image は cosign keyless で digest 署名。`cargo deny`(advisories/unmaintained/yanked/unknown-registry deny、`allow-git=[]`)、`cargo audit` を CI と日次で実行。CodeQL(rust + JS/TS、PR では走らせず master + 日次)、Semgrep(PR、**report-only でゲートしない**)、Trivy(週次、`exit-code: 0`)。
- **コンテナ**: distroless `gcr.io/distroless/cc-debian13:nonroot` を digest pin、UID 65534、`--read-only` 対応。CI が `Config.User == 65534:65534` を検証。docker-compose は host loopback のみ publish、`[::]` bind かつ `allow_public_bind=true`。
  - **注意**: baked な `Dockerfile` の既定 config は `require_pairing=false` + 広い `auto_approve`(Dockerfile:232-241)。pairing 無効時は `/webhook`・`/api/config`・`/api/memory` 等が未認証で応答する(container.md:177-182)。
- **K8s sample**: `runAsNonRoot`/`allowPrivilegeEscalation:false`/`readOnlyRootFilesystem`/`capabilities.drop:[ALL]`/`seccompProfile: RuntimeDefault`。ただし NetworkPolicy・PSA・mTLS manifest は無く、state は `emptyDir`(ephemeral)。single-writer per workspace のため水平スケール不可。

## YOLO mode

`docs/book/src/getting-started/yolo.md` が公式説明。承認プロンプト・workspace 境界・shell ポリシー・allow/denylist・OTP・sandbox のすべてを無効化し、「dev box、home lab、throwaway VM 用。production credential のあるマシンでは実行するな」と警告する。実装は preset `yolo`(`presets.rs:122-140`): `level=Full`、`workspace_only=false`、`allowed_commands=["*"]`、`forbidden_paths=[]`、`block_high_risk_commands=false`、`auto_approve=["*"]`。テストがこれらの不変条件を固定している(presets.rs:704-743)。tool receipts と audit は残せる。

## 制約・弱点(コード上で確認できるもの)

- **docs とコードの乖離**が複数ある: Windows AppContainer は docs のみ、Bubblewrap/Firejail の検出順と docs が不一致、`firejail_args` は config にあるが `FirejailSandbox::new()` に渡されず**未配線**、`forbidden_commands` は system prompt にのみ存在。
- **既定ビルドで Landlock が無効**(feature 非同梱)。自動検出は Firejail/Docker/None に落ち得る。
- **`NoopSandbox` が既定**(`ShellTool::new`)。本番配線を通さないと sandbox なし。
- **`Full` が `workspace_only` を暗黙解除**、`unrestricted_filesystem` も全パス開放。YOLO preset は wildcard + `block_high_risk_commands=false` でシェル構文検査も無効化。
- `allowed_tools = []` が「全許可」を意味し、deny-all には別フラグが必要。
- 読み取りツールも Act レート予算を消費(コメントと矛盾)。`max_cost_per_day_cents` は未強制で、実上限は別系統の `[cost]` が既定 `warn`。
- 未実装/stub: `AuthProvider` レジストリ本番未配線、Nevis local JWKS、`ingress.rs`(常に Loop)、verifiable intent の chain verifier、plugin の多く permission、ADR-014 egress、tool receipts の永続 DB/cross-scope 検証。
- 監査は単一ファイルの SHA-256 ハッシュチェーン(HMAC 署名は任意)で、Merkle でも OS レベルの append-only でもない。ローテーションでファイル名が変わる。
- confusable/ambiguous: `docker-compose.yml` は `allow_public_bind` を「警告を消すだけ」と説明するが `network-deployment.md:28` は「無いと daemon が拒否する」と説明する。

## 検証上の注意(不確実性)

- ソースは master 先頭コミット `d8d5d93` の clone に基づく。sandbox/permission 周りは活発に変更されている可能性があり、記述は取得時点のもの。特定リリースには固定していない。
- 広い範囲を4並列のサブエージェントで要約・統合しており、`policy.rs`(約9,852行)や `cert_ledger.rs` 等の巨大ファイルは主要関数・型の抽出に依拠する。全文を行単位で確認していない箇所がある。
- `.github/workflows` のジョブ挙動、plugin の ADR-014、K8s sample の詳細は要約に依拠する。
- 既定値は `schema.rs` の型定義と `RuntimeProfileConfig`/`SecurityPolicy` の `default()` 実装が複数箇所に散在しており、docs の記述と実装が食い違う箇所がある(本文中に明記した)。実際の値は起動時 config のマージ順に依存する。
- バックエンド挙動は OS・kernel に依存する(Landlock は ABI 依存、Seatbelt は一部 CLI と非互換、Docker は runtime 依存)。
