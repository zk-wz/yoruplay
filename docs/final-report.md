# YoruPlay 最终交付报告

日期：2026-11-01  
目标：Arch Linux / Surface Pro 6 / Niri + Noctalia / PipeWire  
仓库：`~/Projects/yoruplay`

## 1. 交付概况

YoruPlay 已实现为一个 Rust workspace，包含：

- 原生 Rust 网易云 Web API 客户端（eapi/weapi，无 Node.js 运行时依赖）
- 独立后台 daemon、受保护的 Unix socket IPC、单实例锁
- 播放队列与五种播放模式
- 系统 PATH 中 mpv 的 JSON IPC 控制器（不链接 libmpv）
- MPRIS 2.2（`org.mpris.MediaPlayer2.yoruplay`）
- Ratatui + Crossterm TUI、CLI、浏览器 Cookie 登录、Cookie stdin 导入
- state/auth 原子持久化、受限缓存、安装/卸载文档

release 二进制已安装到 `~/.local/bin/yoruplay`。项目不安装 systemd unit、
开机自启项或其他系统注册；TUI/CLI 会按需拉起 daemon。

## 2. 架构与技术取舍

```text
TUI / CLI / MPRIS
        │  Unix socket / 共享命令处理
        ▼
   YoruPlay daemon
   ├── 网易云 API（原生 Rust）
   ├── 唯一权威队列 + 播放模式
   ├── mpv JSON IPC
   └── 持久化 / 缓存 / MPRIS
        │
        ▼
  系统 mpv --ao=pulse
        │
        ▼
   pipewire-pulse
```

关键取舍：

- **单写者状态**：TUI、CLI、MPRIS 都调用同一个 `Daemon::handle_command`；
  没有三套独立队列。
- **播放代际（generation）**：快速切歌时旧 URL 请求完成后会被丢弃，避免旧歌
  覆盖新歌。
- **队列纯状态机**：队列、随机历史、上一首、顺序停止、心动上下文均可用单元
  测试覆盖。
- **mpv 只经 JSON IPC**：不解析终端输出，不模拟键盘；`--no-config`、无视频、
  `--ao=pulse`、`DISABLE_RTKIT=1`。
- **mpv 生命周期**：懒启动；显式 Stop/队列清空/顺序结束/错误停止时立即回收
  mpv；暂停时保留以便快速恢复；daemon 停止时可靠回收。
- **缓存**：只缓存歌词和每日推荐，不缓存音频；TTL + 大小上限 + 原子写入。
- **认证**：`auth.json` 0600，目录 0700；套接字 0600；日志不记录 Cookie。

## 3. API 与上游移植

参考上游：`NeteaseCloudMusicApiEnhanced/api-enhanced`，commit
`984cc021b8598cf94c22134f32a8426887fb2de7`，MIT。详细映射见
`docs/upstream-api-map.md`。

已实现：浏览器 Cookie 登录、账号、用户歌单、喜欢列表、歌单详情/全部歌曲、搜索
（歌曲/歌手/专辑/歌单）、每日歌曲/歌单、私人漫游、私人雷达发现、最近播放、
歌曲详情、音频 URL、歌词、心动模式。

已知差异：不移植 xeapi/neapi/匿名 token 刷新/反作弊/JS 解锁；音频 URL 使用
eapi（上游该模块使用 xeapi），实际请求已验证可用；私人雷达没有独立上游模块，
通过账号每日推荐歌单名称动态发现，不硬编码 ID。

## 4. 已实际执行的验证

### 构建与静态检查

- `cargo fmt --check`：通过
- `cargo clippy --workspace --all-targets`：无警告
- `cargo test --workspace`：**59 个测试通过，3 个需要真实账号/网络的测试被 ignore**
- release 构建：`cargo build --release` 成功，thin LTO + strip，约 11 MiB

### 在线只读 API（匿名）

- 歌曲详情、四种搜索、公开歌单 200 首完整加载、歌词：通过
- 匿名可播放流 URL 获取：通过（免费曲目）
- 试听曲目返回 30 秒 URL 并正确显示试听警告：通过

### 真实音频链路（免费/试听流，无账号）

- 从真实网易云 URL 播放到系统 mpv：通过
- `pactl list sink-inputs` 确认 `application.name = "YoruPlay"`、
  `client.api = "pipewire-pulse"`，采样率 44.1/48 kHz
- mpv 进程参数确认 `--ao=pulse`、`--input-ipc-server=...`、`--no-config`、
  `DISABLE_RTKIT=1`
- TUI 搜索并播放、退出 TUI 后继续播放：通过
- 重新打开 TUI 接管当前播放状态：通过
- 列表循环自动下一首、单曲循环 `loop-file=inf`、顺序播放自动前进并在队尾停止：
  通过
- 随机播放 next/previous 历史：通过
- 快速连续切歌：最终状态一致
- 无账号且无歌单上下文时启用心动模式被明确拒绝；有歌单上下文时模式可切换，
  API 要求登录时返回明确错误，未静默退化为随机播放
- 歌单试听流时长 30 秒时，MPRIS `mpris:length` 正确显示 30 秒

### MPRIS

- `PlaybackStatus`、`Position`（微秒）、`Metadata`、`Volume`、`LoopStatus`、
  `Shuffle`、`Can*` 属性：已验证
- Play/Pause/PlayPause/Stop/Next/Previous/Seek/SetPosition：已验证
- 错误的 Track ID 被拒绝：已验证
- 元数据包含标题、歌手数组、专辑、专辑歌手、封面、时长、稳定 Track ID、
  纯文本歌词 `xesam:asText`：已验证
- Play/Pause/Seek/Volume/Loop 的 `PropertiesChanged`：已验证，且修复了重复信号
- 未做 Noctalia 视觉截图；通过 D-Bus 直接验证了 Noctalia 使用的接口

### 生命周期与持久化

- 单实例锁：第二次 daemon 启动被拒绝
- 服务停止时回收 mpv、移除 socket：通过
- 服务重启恢复 200 首/50 首队列、当前曲目、位置、模式、音量：通过
- Stop / 队列清空立即回收 mpv，下一次 Play 重新启动：通过
- 损坏 state 文件会被移到 `state.json.corrupt-<timestamp>`：单元测试
- TUI 关闭后播放继续；TUI 重新连接成功：通过

### 资源占用（release，PSS）

| 场景 | 总 PSS | 说明 |
|---|---:|---|
| daemon 空闲（匿名空队列，历史） | **6.7 MiB** | RSS 9.9 MiB，9 线程，无 mpv |
| daemon 当前（已登录 + 10 首队列） | **10.3–10.8 MiB** | RSS 13.6–14.1 MiB，10 线程，无 mpv |
| daemon + TUI（历史） | **≈10 MiB** | TUI 3.2 MiB |
| 正常播放（daemon + mpv） | **≈67.2 MiB** | daemon 10.8 MiB + mpv 56.4 MiB |
| 缓存 | 32 MiB 上限 | 浏览后约 60 KiB；无音频缓存 |

满足 <100 MiB 目标；daemon 空闲 CPU 0.0–0.3%。mpv 缓存从 16 MiB / 4 MiB / 60 s
降到 8 MiB forward / 2 MiB back / 30 s，实测 mpv PSS 从约 61.4 MiB 降到
56.4 MiB。歌单元数据缓存限制为 8 项、每项 20,000 首，避免浏览超大歌单导致
内存无界增长。pigma 源码对比记录在 `docs/upstream-parity.md`。

## 5. 登录重构与真实账号验收

歌曲缓存采用原生格式、`~/Music/YoruPlay`、`lofty` 标签，但按计划**尚未实现**；
设计见 `docs/song-cache.md`。

二维码登录已移除。原因是网易在扫码阶段可能根据设备指纹/IP 拒绝二维码，
服务端不会下发 802，客户端无法从 802/803 状态入手修复；继续重试只会增加风控。

当前登录方式：

```sh
yoruplay auth login
```

打开 `https://music.163.com/login`，等待用户在火狐浏览器完成官方登录，然后
由 daemon 读取 Firefox 的 `cookies.sqlite`：

- 自动发现 Arch、上游、Snap、Flatpak 的 Firefox profile
- 复制数据库和 WAL/SHM 到私有临时目录，只查询 `MUSIC_U`、`__csrf` 等
  NetEase Cookie
- 通过 stdin/stdout 之外的内存路径导入，不打印 Cookie
- 调用 `/weapi/nuser/account/get` 验证后写入 0600 的 `auth.json`
- 删除临时数据库

其他浏览器（Chrome/Chromium/Brave/Edge）使用 keyring 加密 Cookie，暂不自动
读取；可用 `yoruplay auth login --cookie-stdin` 手动导入。TUI 登录面板支持 `b`
打开官方页面、`i` 导入 Firefox Cookie。

`yoruplay daemon stop` 保持可用：通过 IPC 优雅关闭 daemon、保存
队列并回收 mpv；兼容旧协议 daemon 的 shutdown 握手。

## 5.1 真实账号验证结果

通过 Firefox 官方网页会话完成 Cookie 导入，并使用
`crates/api/tests/live_account.rs` 做只读账号校验。已验证：

- 账号资料读取
- 创建歌单 10 个、收藏歌单 136 个
- 喜欢音乐 924 首
- 每日推荐歌曲 35 首、每日推荐歌单 6 个
- Personal FM 3 首
- 最近播放 100 首
- 四种搜索类型
- exhigh 320 kbps 非试听流 URL（1200 秒过期）
- 歌词读取

修正了真实响应中的 `playlist: null`、`trackIds: null`、`tracks: null` 和
`name: null` 兼容问题。

仍未完成：

- 心动模式真实推荐：测试歌单返回“不支持该歌单类型”
- 私人雷达：每日推荐列表在不同请求间可能不含雷达条目；daemon 使用缓存列表
- URL 过期后的刷新、真实断网/恢复、Wi-Fi 切换、休眠唤醒、蓝牙重连、
  Noctalia 媒体面板视觉截图

运行：

```sh
cargo test -p yoruplay-api --test live_account -- --ignored --nocapture
cargo test -p yoruplay-api --test live_smoke -- --ignored --nocapture
```

## 6. Git 提交

```text
434c705 perf: bound playlist cache and tighten mpv buffering
d2a3ca7 fix(cli): clean login prompt output
133085e feat(auth): replace QR login with Firefox cookie import
e3cacdd fix(api): tolerate null arrays in real account responses
dcb6e0a docs: record QR login follow-up and shutdown command
6c0ed70 fix(auth): adopt current web QR login and add daemon shutdown
21e5004 fix(auth): use web chainId and stable pre-login device
f7be1d0 docs: add final delivery report
97767c2 fix(player): reap idle mpv and verify real playback path
cedd914 fix: harden MPRIS signals, persistence and TUI edge cases
2b3f709 docs: document setup, upstream mapping and architecture
65e040a feat: implement daemon, MPRIS, CLI and terminal UI
181c2de feat(api): port NetEase crypto, session and core endpoints
7196d03 chore: initialize Rust workspace and shared protocol
```

## 7. 系统环境改动

在用户权限下完成：

- 安装 `~/.local/bin/yoruplay`（release）
- 不安装 systemd unit、不自启、不修改 PATH
- 创建/使用 `~/.config/yoruplay/config.toml`、
  `~/.local/share/yoruplay/state.json`、`$XDG_RUNTIME_DIR/yoruplay/`
- daemon 由 TUI/CLI 按需拉起；测试结束已停止播放
- 测试队列、缓存和账号状态按实际使用保留

未修改：内核、UKI、Secure Boot、PAM/sudoers、系统音频服务、RTKit 限制、
系统网络/代理、其他应用配置、开机启动状态。未使用 root。

## 8. 使用

```sh
yoruplay auth login     # 导入 Firefox Cookie（必要时打开官方登录页）
yoruplay                # TUI
yoruplay daemon status
yoruplay library fm
yoruplay library daily
yoruplay library radar
yoruplay library liked
yoruplay library recent
yoruplay search ...
yoruplay daemon start   # 后台启动 daemon
yoruplay daemon stop    # 关闭当前 daemon
```

详见 `README.md`、`docs/architecture.md`、`docs/upstream-api-map.md`、
`docs/known-issues.md`、`docs/resource-usage.md`。
