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
- Ratatui + Crossterm TUI、CLI、二维码登录、Cookie stdin 导入
- state/auth 原子持久化、受限缓存、systemd 用户服务、安装/卸载文档

release 二进制已安装到 `~/.local/bin/yoruplay`。systemd 用户服务已写入
`~/.config/systemd/user/yoruplay.service`，服务当前运行中，但**未启用开机自启**
（`UnitFileState=disabled`）。

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

已实现：二维码 key/轮询、账号、用户歌单、喜欢列表、歌单详情/全部歌曲、搜索
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

- 二维码 key 生成、轮询 `801`、过期 `800`：通过
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
| daemon 空闲 | **6.7 MiB** | RSS 9.9 MiB，9 线程，无 mpv，cgroup MemoryCurrent 3.2 MiB |
| daemon + TUI | **≈10 MiB** | TUI 3.2 MiB |
| 正常播放（daemon + mpv） | **62–81 MiB** | mpv 60–78 MiB；daemon 2.2–3.3 MiB |
| 播放 + TUI | **≈67 MiB** | TUI 3.2 MiB |
| 试听流 | **68.4 MiB** | mpv 启动峰值 |
| 长时间暂停 30 秒 | 81.6 MiB | Position 完全冻结，恢复成功 |
| 缓存 | 浏览后约 60 KiB | 已清除 |

满足 <100 MiB 目标；daemon 空闲 CPU 0.0–0.3%。mpv 缓存已从 64 MiB 降到
16 MiB forward / 4 MiB back，PSS 从约 112 MiB 降到约 60 MiB。

## 5. 真实账号验收与扫码风控补充

后续排查发现上游 issue #1/#4 记录了同样的“手机提示设备环境异常、API 收不到
二维码状态”问题。上游修复提交 `4f4aa13` 为 Web 二维码增加
`chainId=v1_<deviceId>_web_login_<timestamp>`，并改为生成稳定的匿名 device。
YoruPlay 的首次移植遗漏了 `chainId`，现已修复：

- 二维码 URL 改为 Web 登录链式参数；
- 未登录时也持久化稳定的设备身份（auth.json 0600，无秘密 Cookie）；
- 二维码轮询状态/错误码写入 tracing 日志（不含 key/cookie）；
- 8821/8810 风控错误给出明确提示（不是封号）。

该修复仍需用户实际扫码复测。若仍出现风控，优先让网易云域名绕过 Clash TUN，
或使用官方网页登录后通过 `--cookie-stdin` 导入 MUSIC_U。

## 5.1 未完成的真实账号验收

**本次运行没有完成一次成功的二维码扫码登录**（多次二维码均过期）。因此以下
项目标记为“已实现但未完成真实账号验证”，不能声称通过：

- 二维码登录成功回调、登录后账号资料、会话保存/复用/失效
- 我的歌单、收藏歌单、喜欢音乐
- 每日推荐歌曲/歌单
- 私人漫游、私人雷达实际发现
- 最近播放
- 心动模式真实推荐队列响应解析与连续生成
- 账号可用音质/VIP 曲目
- Cookie 导入成功路径（无效 Cookie 回滚已测试）

其他未复现项：20 分钟 URL 过期后的刷新、真实断网/恢复、Wi-Fi 切换、休眠唤醒、
蓝牙断开重连、Noctalia 媒体面板视觉截图。

用户完成 `yoruplay login` 后，可运行：

```sh
cargo test -p yoruplay-api --test live_account -- --ignored --nocapture
cargo test -p yoruplay-api --test live_smoke -- --ignored --nocapture
```

以完成账号相关验收。

## 6. Git 提交

```text
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
- 写入 `~/.config/systemd/user/yoruplay.service`，**未 enable**
- 创建/使用 `~/.config/yoruplay/config.toml`、
  `~/.local/share/yoruplay/state.json`、`$XDG_RUNTIME_DIR/yoruplay/`
- 当前服务 `active`、`UnitFileState=disabled`
- 测试队列已清空、缓存已清空、没有 auth.json（未登录）、无 mpv 残留

未修改：内核、UKI、Secure Boot、PAM/sudoers、系统音频服务、RTKit 限制、
系统网络/代理、其他应用配置、开机启动状态。未使用 root。

## 8. 使用

```sh
yoruplay login          # 扫码（服务已在运行）
yoruplay                # TUI
yoruplay status
yoruplay fm / daily / radar / liked / recent / search ...
systemctl --user start|stop|restart yoruplay
```

可选启用开机自启（默认不做）：

```sh
systemctl --user enable yoruplay
```

详见 `README.md`、`docs/architecture.md`、`docs/upstream-api-map.md`、
`docs/known-issues.md`、`docs/resource-usage.md`。
