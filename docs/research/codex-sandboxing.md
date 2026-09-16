# codex CLI の sandbox 実装

> 出典調査: [openai/codex](https://github.com/openai/codex) の `codex-rs/sandboxing/src` とその関連クレート(`codex-linux-sandbox`, `windows-sandbox-rs`, `mxc-sandbox`, `network-proxy`, `protocol`)を読み、Linux/macOS/Windows の sandbox 実装とポリシーモデルを参考文献として抽出したメモ。
>
> 検証方法: GitHub `main` ブランチの生ソース(`raw.githubusercontent.com`)を一次情報として直接取得して確認した。リポジトリはローカルに存在しないため、特定コミットには固定していない(取得時点の `main`)。`.sbpl` ポリシーファイルの一部は全文を確認したが、本メモには役割と代表ルールのみを記す。

## 結論

**codex の sandbox は「実行するコマンドの argv を書き換え、OS ネイティブの隔離メカニズムで包んでから spawn する」方式である。** アプリケーション内のパス検査ではなく、カーネルが強制する境界(macOS Seatbelt、Linux Landlock/bubblewrap/seccomp、Windows トークン/MXC)を最終防衛線に置く。ポリシーは **deny-by-default** で、`SandboxManager` がリクエストを検証し、プラットフォームごとのラッパー argv を生成する。

| 軸 | 実装 |
|---|---|
| ポリシー表現 | legacy `SandboxPolicy`(read-only / workspace-write / danger-full-access)を互換ビューとして残しつつ、二軸の `PermissionProfile`(filesystem × network)へ移行中 |
| バックエンド選択 | `SandboxType`(None / MacosSeatbelt / LinuxSeccomp / WindowsRestrictedToken / WindowsMxc) + `get_platform_sandbox` |
| macOS | `/usr/bin/sandbox-exec` に SBPL プロファイルを `-p`/`-D` で渡す |
| Linux | bubblewrap 優先。`codex-linux-sandbox` ヘルパーを arg0 自己 exec で再入し、Landlock/seccomp は legacy フォールバック |
| Windows | restricted token(非昇格/昇格)と、Microsoft MXC ネイティブ実行の2系統 |
| ネットワーク | 既定は遮断。managed proxy 時は netns/proxy 経由の許可リストにフォールバックし、失敗時は fail-closed |
| 拒否検知 | 実行後の出力をキーワード走査する軽量ヒューリスティクス + 構造化 violation イベント(tracing warn) |

## 全体アーキテクチャ

sandbox 層は独立クレート `codex-sandboxing` にある(`Cargo.toml` の依存は `codex-protocol`, `codex-network-proxy`, `codex-utils-pty`, `codex-windows-sandbox`, `codex-mxc-sandbox`, `which`, `url` など)。プラットフォーム固有の実ヘルパーは別クレートに分かれ、`codex-sandboxing` はそれらを束ねるディスパッチャとして振る舞う。

### `SandboxType` とプラットフォーム選択

`manager.rs` で定義される。メトリクスタグも併せて持つ。

| `SandboxType` | `as_metric_tag` | 意味 |
|---|---|---|
| `None` | `none` | sandbox なし |
| `MacosSeatbelt` | `seatbelt` | macOS sandbox-exec |
| `LinuxSeccomp` | `seccomp` | Linux(codex-linux-sandbox 経由。bwrap/Landlock/seccomp) |
| `WindowsRestrictedToken` | `windows_sandbox` | Windows restricted token |
| `WindowsMxc` | `windows_mxc` | Microsoft MXC ネイティブ |

`get_platform_sandbox(windows_sandbox_enabled)` は `cfg!` で OS を判定し、Windows のときだけ `windows_sandbox_enabled` フラグで有無を切り替える。macOS/Linux では常にバックエンドを返す。

### サンドボックスの要否: `SandboxablePreference`

`SandboxablePreference::{Auto, Require, Forbid}` を `SandboxManager::should_sandbox` が評価する。

- `Forbid` → 常に `false`(sandbox しない)。
- `Require` → 常に `true`(ホストにバックエンドがあれば強制)。
- `Auto` → `should_require_platform_sandbox(file_system_policy, network_policy, has_managed_network_requirements)` に委譲(後述)。

`select_initial(profile, pref, windows_sandbox_level, has_managed_network_requirements)` は、要否判定が真なら `get_platform_sandbox(...)` の結果を返し、偽なら `SandboxType::None` を返す。

### 変換の中心: `SandboxManager::transform`

`SandboxTransformRequest`(コマンド、権限プロファイル、sandbox 種別、managed network、cwd、`sandbox_exe` など)を受け取り、`SandboxExecRequest`(実行すべき argv、`sandbox_policy_cwd`、env、`arg0`、実効 `permission_profile` など)を返す。分岐の要点:

- **`None`** — argv をそのまま通す。
- **`MacosSeatbelt`** — `create_seatbelt_command_args_with_profile` でラッパー argv を生成し、`/usr/bin/sandbox-exec` を先頭に付ける。macOS 以外では `SeatbeltUnavailable`。
- **`LinuxSeccomp`** — `sandbox_exe`(codex-linux-sandbox)必須。`create_linux_sandbox_command_args_for_permission_profile` で引数を生成し、`arg0` に `codex-linux-sandbox` を設定。`ensure_linux_bubblewrap_is_supported` で WSL1 を弾く。
- **`WindowsRestrictedToken`** — managed network は昇格バックエンド必須。Windows 以外では素通し。
- **`WindowsMxc`** — private desktop を拒否、`is_available()` 必須、executor ローカル proxy が必要。

cwd は `PathUri` で受け、実行境界でネイティブパスへ変換する(`PendingSandboxedExecRequest`)。変換失敗は `SandboxTransformError`(下記)に集約され、`From<SandboxTransformError> for CodexErr`(`lib.rs`)で `CodexErr::InvalidRequest` / `LandlockSandboxExecutableNotProvided` / `UnsupportedOperation` などにマップされる。

| `SandboxTransformError` | 発生条件 |
|---|---|
| `InvalidCommandCwd` / `InvalidSandboxPolicyCwd` | `PathUri` → ネイティブパス変換失敗 |
| `MissingLinuxSandboxExecutable` | Linux で `sandbox_exe` 未指定 |
| `WindowsMxcPreparation(String)` | MXC 準備失敗(private desktop / 未対応 / proxy 不在) |
| `EnvironmentNetworkProxy(String)` | managed proxy 準備失敗 |
| `SeatbeltPreparation(String)` | macOS SBPL 生成失敗(symlink など) |
| `Wsl1UnsupportedForBubblewrap` | WSL1 で bwrap が必要 |
| `SeatbeltUnavailable` | macOS 以外で Seatbelt 指定 |
| `WindowsSandboxPreparation(String)` | Windows ラッパー準備失敗 |

### spawn 経路

`spawn.rs` の `spawn_process(SpawnRequest)` が最終実行を担う。

```rust
pub struct SpawnRequest<'a> {
    pub command: &'a [String],
    pub cwd: &'a Path,
    pub env: &'a HashMap<String, String>,
    pub arg0: &'a Option<String>,
    pub sandbox: SandboxType,
    pub windows_sandbox: Option<WindowsSandboxSpawnRequest<'a>>,
    pub tty: bool,
    pub stdin_open: bool,
    pub inherited_fds: &'a [i32],
}
```

Unix では `codex_utils_pty` に委譲し、`tty` なら PTY、`stdin_open` なら pipe、それ以外は stdin なし pipe で spawn する(`env_clear` 後に `envs`)。`arg0` は明示的に上書きされ、`codex-linux-sandbox` 人格へ切り替える。Windows では `codex_windows_sandbox::spawn_windows_sandbox_session_for_level` に委譲する。

### 端末クエリ応答

`terminal_queries.rs` は端末エミュレーションをせず、ブロックしうる最小のクエリに固定応答する。`QUERY_RESPONSES` は Device Status Report(`ESC[5n` → `ESC[0n`)、window size(`ESC[18t` → `ESC[8;24;80t`)、cursor position(`ESC[6n` → `ESC[1;1R`)の3種。DEC private mode クエリ(`ESC[?<digits>$p`)には「未認識」として応答する。`respond_to_terminal_queries` が stdout の受信ループにステートマシンを挟み、応答を stdin へ書き戻す。

## ポリシーモデル

### legacy `SandboxPolicy`(互換ビュー)

`protocol/src/protocol.rs` の `SandboxPolicy` は設定・ワイヤ上の表現として残る。

```rust
pub enum SandboxPolicy {
    DangerFullAccess,
    ReadOnly { network_access: bool },
    ExternalSandbox { network_access: NetworkAccess },
    WorkspaceWrite {
        writable_roots: Vec<AbsolutePathBuf>,
        network_access: bool,
        exclude_tmpdir_env_var: bool,
        exclude_slash_tmp: bool,
    },
}
```

`has_full_disk_read_access` は legacy では常に真、`has_full_disk_write_access` は `DangerFullAccess`/`ExternalSandbox` のみ真。`get_writable_roots_with_cwd` が cwd、`/tmp`、`$TMPDIR` と保護 read-only サブパスを補う。

### 二軸モデル: `PermissionProfile` + `FileSystemSandboxPolicy` / `NetworkSandboxPolicy`

実効的なモデルは「ファイルシステム × ネットワーク」の二軸で、legacy はその射影として扱われる。

```rust
pub enum PermissionProfile {
    Managed { file_system: ManagedFileSystemPermissions, network: NetworkSandboxPolicy },
    Disabled,
    External { network: NetworkSandboxPolicy },
}

pub enum FileSystemSandboxKind { Restricted /*default*/, Unrestricted, ExternalSandbox }
pub enum NetworkSandboxPolicy { Restricted /*default*/, Enabled }
```

`FileSystemSandboxPolicy` は `kind` と `entries: Vec<FileSystemSandboxEntry>` を持ち、各 entry は `path`(`Path` / `GlobPattern` / `Special`)、`access`(`Read` / `Write` / `Deny`)、`missing_path_behavior` からなる。アクセス優先順位は **deny > write > read**。`FileSystemSpecialPath` には `Root`, `Minimal`, `ProjectRoots`, `Tmpdir`, `SlashTmp`, `Unknown` がある。組み込みプロファイル id は `:read-only` / `:workspace` / `:danger-full-access`。

保護メタデータとして `PROTECTED_METADATA_PATH_NAMES = [".git", ".agents", ".codex"]` が writable root 配下で read-only に固定される。

### legacy ↔ 新モデルの対応

- forward: `DangerFullAccess` → `Unrestricted` + network Enabled、`ExternalSandbox` → `ExternalSandbox`、`ReadOnly` → `Restricted([Root=Read])`、`WorkspaceWrite` → `workspace_write(...)`。
- reverse(`to_legacy_sandbox_policy`): workspace 外への書き込みや `:root = write` + carveout は legacy で表現できず `Err(InvalidInput)` になる。この場合は `compatibility_workspace_write_policy` が `WorkspaceWrite` を合成して互換を保つ。

### 実効プロファイルと要否判定(`policy_transforms.rs`)

```rust
pub fn effective_permission_profile(
    permission_profile: &PermissionProfile,
    additional_permissions: Option<&AdditionalPermissionProfile>,
) -> PermissionProfile
```

per-command の追加権限(`AdditionalPermissionProfile`)を元プロファイルにマージする。ファイルシステムの追加は `Restricted` のときのみ効き、`Unrestricted`/`ExternalSandbox` は決して広げない。ネットワークは `enabled == true` が1つでもあれば `Enabled`。

```rust
pub fn should_require_platform_sandbox(
    file_system_policy: &FileSystemSandboxPolicy,
    network_policy: NetworkSandboxPolicy,
    has_managed_network_requirements: bool,
) -> bool
```

- managed network 要件があれば無条件 `true`。
- ネットワーク遮断時: `ExternalSandbox` 以外なら `true`。
- ネットワーク開放時: `Restricted` かつ full-disk write でなければ `true`。

つまり「`Root=Write` かつ carveout なし かつ network 開放」は sandbox 不要と判定される。

## macOS Seatbelt

### 実行の形

`seatbelt.rs` の定数は PATH 探索を避けて絶対パスに固定する。

```rust
pub const MACOS_PATH_TO_SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";
```

argv は「`-p <ポリシー文字列>` → パラメータごとの `-D<KEY>=<VALUE>` → `--` → 元コマンド」の順。パスは `(param "NAME")` で参照させる。Rust 側は argv を組み立てるだけで、spawn は呼び出し側が行う。

### プロファイル種別 `MacosSeatbeltProfile`

| 変種 | 用途 |
|---|---|
| `Process`(既定) | 通常プロセス。`/Applications` と `/tmp` 系のスクラッチへ互換アクセスを与える platform defaults を追加 |
| `FileSystemHelper` | ファイルシステムヘルパー。スクラッチ等を継承させず、明示された権限のみ |

### ポリシー合成

`create_seatbelt_command_args_with_profile` が中心で、`policy_sections` を「base → read → write → network → (preferences) → (read-only platform defaults) → (process platform defaults) → unreadable glob deny → protected ancestor deny」の順に連結する。主な `.sbpl`:

| ファイル | 役割 |
|---|---|
| `seatbelt_base_policy.sbpl` | `(deny default)` から始める。`process-exec`/`process-fork`、`same-sandbox` への signal、sysctl read、IOKit、pseudo-tty、`/dev/null` 書き込み等の最小許可 |
| `seatbelt_network_policy.sbpl` | ネットワーク開放時の AF_SYSTEM、mach-lookup、`net.routetable` sysctl |
| `seatbelt_read_only_platform_defaults.sbpl` | `:minimal` 相当。`/usr/lib`、`/System/Library/Frameworks`、`/dev`、`/etc` などの読み取り・ロード許可 |
| `seatbelt_preferences_policy.sbpl` | cfprefsd 連携。**full-disk read のときのみ**同梱(制限付き read では含めない) |

### read / write / deny の変換

- writable root はディレクトリなら `(subpath ...)`、非ディレクトリ/デバイスなら `(literal ...)`。ファイル root に子孫権限を与えない。
- **writable root 差し替え防止**: ディレクトリ root には `(deny file-write-unlink (require-all (literal (param "WRITABLE_ROOT_N")) (vnode-type DIRECTORY)))` を追加し、root 自体の rename/unlink を塞ぐ(次の sandbox ポリシーが使う権威境界を奪わせない)。
- 除外 carveout は `require-not (literal ...)` と `require-not (subpath ...)` の両方を出す(前者がないと保護ディレクトリ自体の初回 `mkdir` が通る)。
- 保護メタデータは `^<root>/<name>(/.*)?$` の正規表現 deny で、存在前の `.git`/`.codex` 作成も塞ぐ。
- full-disk write は `(allow file-write* (regex #"^/"))`、full-disk read は `(allow file-read*)`。unreadable roots がある場合は root `/` + 除外で表現する。
- 保護 ancestor の rename 対策(`(deny file-write-unlink ...)`)は**最後**に置き、後続の allow で再び開かないようにする。

### glob と symlink 防御

unreadable glob は git 風サブセット(`*`, `**`, `?`, `{a,b}`, `[abc]`)を SBPL 正規表現へ変換する(`seatbelt_regex_for_glob`)。マッチした祖先ディレクトリの unlink も deny する。

writable root の symlink は `nested_symlink_component` で検査し、トップレベル macOS エイリアス(`/tmp` → `/private/tmp`)は許しつつ、ユーザー制御成分の symlink は `SeatbeltPreparationError::FileSystem` で拒否する。`allowed_symlinked_codex_home` を opt-in した場合のみ、`CODEX_HOME` 配下の writable root で symlink 追従を許可する。テストでは `rm -rf $PWD && ln -s target $PWD` が失敗することが確認されている。

### ネットワークと managed proxy

`ProxyPolicyInputs { ports, has_proxy_config, allow_local_binding, unix_domain_socket_policy }` で分岐する。proxy が絡む場合は loopback の bind/inbound/outbound と proxy ポートのみ許可し、DNS(`*:53`)を必要時に追加、Unix socket は `AllowAll` かパス allowlist(`subpath`)。proxy 設定があるのに有効な loopback が取れない、または managed network 強制でエンドポイントがない場合は**空ポリシーを返し fail-closed にする**。proxy ポートは `PROXY_URL_ENV_KEYS` の env から loopback かつ既定ポート(https=443, socks*=1080, その他=80)のみ抽出する。

`MacosSeatbeltProfile::FileSystemHelper` と `allowed_symlinked_codex_home` は `SeatbeltPreparationError::{FileSystem, EnvironmentNetworkProxy}` の2変種だけを持ち、公開 API では `String` に潰される。

## Linux

Linux は**実ヘルパーが別クレート `codex-linux-sandbox`** にあり、`codex-sandboxing/src/{landlock.rs,bwrap.rs,spawn.rs}` は薄いラッパーという構造になっている。ファイルシステム制限は **bubblewrap 優先**で、Landlock は legacy/バックアップ、seccomp はネットワーク制限に使う。

### arg0 自己 exec トリック

単一バイナリが複数の人格を持つ。`CODEX_LINUX_SANDBOX_ARG0 = "codex-linux-sandbox"` が定数。`arg0` クレートが `$CODEX_HOME/tmp/arg0` に `codex-linux-sandbox` → `current_exe()` の symlink を作り、`PATH` 先頭に追加する。`argv[0]` の basename が `codex-linux-sandbox` なら `codex_linux_sandbox::run_main()` にディスパッチする。bwrap に `--argv0` がある場合は `--` の手前に `--argv0 codex-linux-sandbox` を差し込み、ない場合は `--` 直後の実行パスを symlink パスに書き換えて basename を保つ。

### 2段階実行

- **外側**: `--apply-seccomp-then-exec` を付けずに bwrap を起動し、内側コマンド(`current_exe` + `--sandbox-policy-cwd`/`--permission-profile <json>`/`--managed-network <json>`/`--proxy-route-spec <json>` + `--` + ユーザーコマンド)を構築する。
- **内側**(bwrap の後): `--apply-seccomp-then-exec` を確認し、fd マウント検証、`SYS_capget` で capability 非保持を確認、proxy ルートの有効化、seccomp/no_new_privs の適用の後 `fork` + `execvp` し、シグナル転送と reap を行う。

### bubblewrap vs legacy Landlock

`run_main` の流れ:

1. `--permission-profile` を解決(なければ panic)。
2. `has_full_disk_write_access() && !allow_network_for_proxy` なら bwrap 不要で seccomp だけ適用。
3. `!use_legacy_landlock` なら **bwrap 経路**(失敗時の Landlock フォールバックなし)。
4. `use_legacy_landlock` なら Landlock 経路。

`ensure_linux_bubblewrap_is_supported` は `allow_network_for_proxy || (!use_legacy_landlock && !has_full_disk_write_access)` のとき bwrap を要求し、WSL1 では `Wsl1UnsupportedForBubblewrap` を返す。

Landlock(legacy)は `ABI::V5` で `AccessFs::from_all/from_read` を使い、`/` を read-only、`/dev/null` と writable roots を write 可能にし、`set_no_new_privs(true)` の上で `restrict_self` する。制限付き read-only は非対応。

### seccomp

`install_network_seccomp_filter_on_current_thread(mode, managed_network)` が `seccompiler` でフィルタを設置する。既定 Allow、マッチ時 `EPERM`。常時拒否は `io_uring_*` と `ptrace` 系。モード:

| モード | 内容 |
|---|---|
| `Restricted` | `connect`/`accept`/`bind`/`listen` 等を拒否。`socket`/`socketpair` は arg0 == `AF_UNIX` のみ許可 |
| `ProxyRouted` | `socket` は `AF_INET`/`AF_INET6` のみ、`socketpair` は `AF_UNIX` のみ |
| `VmSocketRestricted` | `AF_VSOCK` のみ拒否(WSL2 interop 対策) |

`should_install_network_seccomp` は「ネットワーク遮断」または「proxy 経由許可」で真になり、managed network は `DangerFullAccess` でも fail-closed を保つ。

### bwrap 引数生成と警告

`create_bwrap_command_args` は `--ro-bind / /`(full read)または `--tmpfs /` + 限定 `--ro-bind`(restricted read)、`--dev /dev`、writable root の `--bind`、`.git`/`.agents`/`.codex` の `--ro-bind`、`--unshare-user/pid/ipc`、必要時 `--unshare-net`、`--cap-drop ALL`、`--chdir` を組み立てる。full-disk write かつ unreadable glob なしならコマンドをそのまま返す。unreadable glob は `rg --files` で展開(上限 8192、root 直下 glob は拒否)。

`find_system_bwrap_in_path` は PATH から bwrap を探し、**cwd 配下の候補を除外**してワークスペース内の悪意ある bwrap を防ぐ。システム bwrap が `--ro-bind-fd` を持たない場合は `--ro-bind /proc/self/fd/<fd>` + `--verify-fd-mount` に書き換える。システム bwrap がなければ同梱 bwrap を使い、SHA-256 検証失敗時は exit code 8。

警告(`bwrap.rs`): `is_wsl1()` が `/proc/version` を解析。`system_bwrap_warning` は WSL1、bwrap 未検出、user namespace 不可(`USER_NAMESPACE_FAILURES` のメッセージ一致)を返す。判定不能・タイムアウトは警告なし。

### 環境変数スクラブ

`EnvironmentVariablePolicy` ではなく `ShellEnvironmentPolicy` + `populate_env`。inherit(`All`/`None`/`Core`)、既定除外(`*KEY*`/`*SECRET*`/`*TOKEN*`)、custom excludes、overrides、`include_only`、`CODEX_THREAD_ID` 注入の順。さらに `NON_INHERITABLE_ENV_VARS`(`CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN` 等)は明示設定でも除去する。`~/.codex/.env` から `CODEX_` 接頭辞は設定できない。

### ネットワーク proxy の netns ブリッジ

`LD_PRELOAD` は使わない。`proxy_routing.rs` がホスト側で loopback proxy へのブリッジを `fork` し(`PR_SET_PDEATHSIG`、stdio を `/dev/null` へ)、netns の内側で loopback リスナを SCM_RIGHTS で渡してから proxy env を `127.0.0.1:<local_port>` に書き換える。`ProxyRouteSpec` は実 proxy URL を含めない。元の proxy env キー(`PROXY_ENV_KEYS`)はこの経路で扱う。

## Windows

Windows は `SandboxType::WindowsRestrictedToken`(restricted token)と `SandboxType::WindowsMxc`(Microsoft MXC)の2系統。AppContainer ではない。

### restricted token

`WindowsSandboxLevel::{Disabled, RestrictedToken, Elevated}` で選択する(`Mxc` は level ではなく実行系で、`SandboxType::WindowsMxc` に直行する)。

| level | 実装 |
|---|---|
| `RestrictedToken`(非昇格) | 現在のトークンから `CreateRestrictedToken`(`DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED`)で制限トークンを作り子を直接 spawn。read 制限は不可 |
| `Elevated` | サンドボックス用ユーザーを作り、`codex-command-runner` をそのユーザーで起動。IPC パイプで framed `SpawnRequest` を渡し、ConPTY/pipe で子を spawn。WFP ファイアウォールも設定 |

managed network と deny-read は昇格バックエンドを要求し、非昇格では `"... requires the elevated Windows sandbox backend"` で bail する。wrapper は `codex.exe --run-as-windows-sandbox ...` の形(`CODEX_WINDOWS_SANDBOX_ARG1`)で、`--permission-profile`、`--env-json`、`--workspace-root`、read/write roots と deny paths の override、`--proxy-enforced`、`--network-proxy-restricting-sid` などを取る。private desktop(`CodexSandboxDesktop-*`)と DACL 制御も行う。

`resolve_windows_restricted_token_filesystem_overrides` / `resolve_windows_elevated_filesystem_overrides` が split policy を legacy 射影と比較し、表現できない差異(deny-read、root read 制限、reopened writable descendant など)があれば `"... refusing to run unsandboxed"` を返す。proxy の per-route SID は restricting SID として渡し、DACL には含めない(所有がオブジェクトアクセス権にならないようにする)。

### MXC

`codex-rs/mxc-sandbox`(package `codex-mxc-sandbox`)。`CODEX_WINDOWS_MXC_ARG1 = "--__codex-windows-mxc"`。型付きリクエスト `MxcCommand` を launcher 専用 env(`CODEX_MXC_LAUNCH_*`、chunk 分割、上限 1MB)にエンコードし、Codex 実行ファイルを再入して Microsoft MXC `BaseContainerRunner` に渡す。`is_available()` は PSEC(process security environment)の create/close を実際に probe する。deny path は `supports_deny_paths()` を要求する。private desktop 非対応、`allow_local_binding=false` の managed network 非対応、ボリューム root の grant は非再帰、など制約が多い。

## 拒否検知と violation イベント

### 軽量ヒューリスティクス `denial.rs`

`is_likely_sandbox_denied(sandbox_type, exec_output)` は副作用なしの真偽判定。

- `SandboxType::None` または exit 0 なら偽。
- `is_likely_executor_managed_sandbox_denied`(stderr/stdout/aggregated を小文字化し、`SANDBOX_DENIED_KEYWORDS` の7語 `operation not permitted` / `permission denied` / `read-only file system` / `seccomp` / `sandbox` / `landlock` / `failed to write file` のいずれかを含む)なら真。
- `QUICK_REJECT_EXIT_CODES = [2, 126, 127]` は**キーワード判定の後**に偽とする(素の `127 command not found` は偽、`127 Permission denied` は真)。
- `LinuxSeccomp` かつ exit == `128 + SIGSYS` なら真。

### 構造化 violation `violation.rs`

`classify_filesystem_sandbox_violation` が exit code(0 と `SandboxType::None` は対象外)とキーワードから `FileSystemSandboxViolationReason` を決め、`SANDBOX_DENIED_KEYWORDS` と同じ7語を理由(`OperationNotPermitted`/`PermissionDenied`/`ReadOnlyFileSystem`/`PolicyDenied`/`FailedToWriteFile`)へ写像する。denied path を `": operation not permitted"` 等の接尾辞から抽出し、output snippet は最大512文字。`LinuxSeccomp` の `SIGSYS` は `SignalSyscall`。

公開型は `SandboxViolationEvent::{FileSystem, Network}`、`SandboxViolationBackend::{LinuxSandbox, ManagedNetworkProxy, Seatbelt, WindowsSandbox, WindowsMxc}`、`FileSystemSandboxViolation`、`NetworkSandboxViolation`(proxy の `BlockedRequest` から生成)。

`record_filesystem_sandbox_violation` / `record_network_sandbox_violation` / `record_sandbox_violation` は**最終的に `tracing::warn!` を出すだけ**で、コールバックやメトリクス、別シンクは持たない。`core/src/exec.rs` の `finalize_exec_result` が拒否時に記録し(`SensitiveFullBuffer` 以外)、`CodexErr::Sandbox(SandboxErr::Denied { output, .. })` を返す。

## 検証上の注意(不確実性)

- ソースは GitHub `main` の生ファイルを webfetch で取得したもので、**特定コミットに固定していない**。sandbox 周りは活発に変更されており(permission profile への移行、`PathUri` 化、MXC 追加など)、記述は取得時点のものである。
- `codex-sandboxing` クレートを中心に、関連クレートの主要ファイルを読んだ。`windows-sandbox-rs` の内部(トークン生成、setup、ACL、WFP)と `mxc-sandbox` の native 層、`network-proxy` の内部はサブエージェント経由の要約に依拠しており、全文を直接確認していない。
- `.sbpl` は `seatbelt.rs` から取得した全文と、各ファイルの役割・代表ルールを確認した。本メモでは全文転記を省略している。
- `SandboxPolicy` と `PermissionProfile` の二重モデルは移行途中で、legacy 逆変換に失敗する権限(`:root = write` + carveout 等)が存在する。将来この互換層が消える可能性がある。
- バックエンドの挙動は OS バージョン依存(macOS Seatbelt は Apple 非公式サポート、Landlock は ABI 依存、MXC は Windows build の PSEC 対応依存)で、コード上の分岐と実挙動が一致しない場合がある。
- メトリクス名(`codex.windows_sandbox.*`, `codex.windows_mxc.available` 等)や `tracing` 出力形式は変更されうる。
