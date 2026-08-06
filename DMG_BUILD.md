# 打包 aidea.dmg（方案 Y：Linux 交叉编译 + 本机封包）

## 为什么要分两步

本机 macOS 的 Spotlight 元数据服务（`mds`）已损坏，导致**任何形式的 `cargo build`（包括 `--offline`）
都会在依赖解析阶段死锁**，无法在本机编译。

但 `hdiutil` / `codesign` / `ditto` / `sips` / `iconutil` **不依赖 mds**，工作完全正常。

所以拆成两段：

| 阶段 | 在哪跑 | 干什么 |
| --- | --- | --- |
| 编译 | GitHub Actions **ubuntu-latest**（免费，无需绑卡） | osxcross 交叉编译出 `x86_64-apple-darwin` 原生二进制 + vite 构建前端 + 抓 ollama/PostgreSQL 的 macOS 预编译件，打成 `aidea-bundle.tar.gz` |
| 封包 | **本机 macOS** | `package-dmg.sh` 组装 `.app` → ad-hoc 签名 → `hdiutil` 出 `aidea.dmg` |

CI 里**故意不跑 `tauri build`**：Linux 生成不了 macOS 的 `.app/.dmg`，而且会再次触发 cargo。

---

## 三步拿到 dmg

### 1. 触发 CI

GitHub 仓库 → **Actions** → 左侧选 **Build aidea macOS bundle (cross-compile)** → **Run workflow**。

可选参数：

| 参数 | 默认 | 说明 |
| --- | --- | --- |
| `bake_model` | `true` | 把 `qwen2.5-0.5b.gguf` 端模型权重烘焙进包（离线可用，artifact 约 +400MB）。关掉则首次启动需联网让 ollama 拉权重 |
| `osxcross_tag` | `14.5-r0-ubuntu` | `crazymax/osxcross` 镜像 tag（提供 MacOSX SDK + cctools/ld64）。SDK 太新导致链接报错时可降到 `13.1-r0-ubuntu` |

push 到 `main` / `master` 也会自动触发。

### 2. 下载产物

运行结束后，在该次运行页面底部 **Artifacts** 区下载 `aidea-bundle`（一个 zip），
解压后得到 **`aidea-bundle.tar.gz`**（里面已经附带了一份 `package-dmg.sh`，解包即用）。

### 3. 本机封包

把 `aidea-bundle.tar.gz` 和 `package-dmg.sh` 放在同一目录，执行：

```bash
bash package-dmg.sh aidea-bundle.tar.gz
```

不带参数时脚本会自动在当前目录和 `~/Downloads` 里找 `aidea-bundle.tar.gz`。

完成后当前目录得到 **`aidea.dmg`**。

---

## 首次打开

`tauri.conf.json` 里 `bundle.macOS.signingIdentity = null`，所以用的是 **ad-hoc 签名**（`codesign --sign -`），
没有 Apple 开发者证书。首次打开可能提示「无法打开，因为无法验证开发者」，任选一种放行：

* Finder 里**右键**点 `aidea.app` → **打开** → 弹窗里再点一次「打开」
* 或执行：`xattr -dr com.apple.quarantine /Applications/aidea.app`
* 或：系统设置 → 隐私与安全性 → 滚到底部点「仍要打开」

---

## 脚本可选环境变量

```bash
OUT_DIR=~/Desktop bash package-dmg.sh aidea-bundle.tar.gz   # 换输出目录
OUT_NAME=aidea-0.1.0.dmg bash package-dmg.sh                # 换文件名
KEEP_WORK=1 bash package-dmg.sh                             # 保留临时工作区便于排查
DRY_RUN=1 bash package-dmg.sh aidea-bundle.tar.gz           # 只组装 .app 目录并打印结构，不签名不出 dmg
SKIP_SIGN=1 bash package-dmg.sh                             # 跳过签名（仅排障）
MIRROR_MACOS_BIN=0 bash package-dmg.sh                      # 不在 Contents/MacOS/ 建 sidecar 软链
```

`DRY_RUN=1` 可以在任意平台（包括 Linux）跑，用来验证目录拼装逻辑。

---

## 生成的 .app 结构（以及为什么是这个结构）

```
aidea.app/Contents/
├── Info.plist                     CFBundleExecutable=aidea-gui, id=com.aidea.gui
├── MacOS/
│   ├── aidea-gui                  主二进制
│   └── aidea, ollama, postgres,   → ../Resources/bin/<name> 的相对软链
│       initdb, pg_ctl, psql         （兼容 Tauri 官方 bundler 约定，可用 MIRROR_MACOS_BIN=0 关闭）
└── Resources/
    ├── bin/                       6 个 sidecar（**去掉 triple 后缀**）
    ├── lib/                       postgres 的 dylib + ollama 的 runner（lib/ollama/）
    ├── share/                     postgres initdb 模板等
    ├── resources/
    │   ├── migrations/*.sql       数据库迁移
    │   └── models/nes-tab/        Modelfile + 可选 qwen2.5-0.5b.gguf
    ├── dist/                      前端静态文件（留档；Tauri v2 已在编译期内嵌进二进制）
    ├── icons/icon.png
    └── icon.icns                  由 sips + iconutil 从 icon.png 生成
```

### 关键设计依据

1. **sidecar 放 `Contents/Resources/bin/`，不是 `Contents/MacOS/`。**
   本项目**没有**用 Tauri 的 `Command::new_sidecar()`，而是在
   `gui/src-tauri/src/bootstrap/sidecar.rs` 里自己解析路径：

   ```rust
   pub fn bin_path(app: &AppHandle, name: &str) -> PathBuf {
       resource_path(app, &format!("bin/{name}"))   // = resource_dir()/bin/<name>
   }
   ```

   macOS 上 `resource_dir()` 就是 `aidea.app/Contents/Resources`，所以真实文件必须落在
   `Contents/Resources/bin/<name>`，且**不带** `-x86_64-apple-darwin` 后缀。
   `Contents/MacOS/` 下只建软链兜底。

2. **必须连 `lib/` 和 `share/` 一起打包。**
   Postgres.app 的 `postgres`/`psql` 用 `@loader_path/../lib` 引用自带 dylib，
   `initdb` 按 `<exe>/../share` 找模板；ollama 按 `<exe>/../lib/ollama` 找推理 runner。
   把 `bin` / `lib` / `share` 都放在 `Contents/Resources/` 下互为兄弟目录，三者的相对查找同时成立。
   （只拷 `bin/*` 的老做法会让 .app 在没装 Homebrew 的机器上直接起不来。）

3. **`resources/` 前缀要保留。**
   Rust 侧读的是 `resource_path(app, "resources/migrations")` 和
   `resource_path(app, "resources/models/nes-tab/Modelfile")`，
   所以是 `Contents/Resources/resources/...`（两层 resources 是对的，别拍平）。

4. **Modelfile 里写的是 `FROM ./qwen2.5-0.5b.gguf`**，权重必须与 Modelfile **同目录同名**。

---

## 排障

| 现象 | 原因 / 处理 |
| --- | --- |
| CI 在 "Smoke-test osxcross" 失败 | osxcross 镜像结构变了或 SDK 没拷到。换 `osxcross_tag`（如 `13.1-r0-ubuntu` / `15.5-r0-ubuntu`）重试 |
| CI 在 cargo 交叉编译失败 | 看 `cargo-logs` artifact。常见是 SDK 缺 framework 头文件 → 换更高版本 SDK tag；或 OOM → 工作流已自动关 LTO 重试一次 |
| CI 警告"未能从 dmg 中提取 Versions/17/bin" | Postgres.app 换了 dmg 分区格式。可改用 `theseus-rs/postgresql-binaries` 的 `x86_64-apple-darwin` tar.gz（见工作流注释里的备选源） |
| `package-dmg.sh` 报"关键 sidecar 缺失：aidea" | CI 的 Rust 交叉编译没成功，bundle 里没有二进制。先修 CI |
| 应用启动报"sidecar 二进制缺失" | 打开 `aidea.app/Contents/Resources/bin/` 看文件在不在、有没有 `+x` |
| 应用启动报数据库不可用 | `Contents/Resources/lib` / `share` 缺失，说明 CI 那步降级了；看 CI 日志里的 warning |
| dmg 能装但闪退 | 终端里跑 `/Applications/aidea.app/Contents/MacOS/aidea-gui` 看 stderr |
