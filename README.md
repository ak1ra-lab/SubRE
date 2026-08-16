# subtitle-renamer

桌面端字幕批量重命名工具. 基于集数 key 自动配对视频与字幕文件, 把外挂字幕改名为视频同名(可配置后缀)以便播放器自动载入.

技术栈: Rust + egui(eframe). 核心逻辑 (`core` 模块) 与 GUI 完全分离, 可独立单测. 运行时数据: sqlite 历史库(用户数据目录); 配置: TOML(用户配置目录). 产物为单文件静态二进制.

## 特性

- **自动配对**: 基于文件名集合内差异片段提取集数 key, 无需固定命名格式. 支持正则兜底与手动指派.
- **目标名生成**: 视频主名 + 可选后缀 + 字幕原扩展名. `.idx` + `.sub` 成对作为同一逻辑单元.
- **后缀三级解析**: 用户标记映射表 → 文件名语言 token 自动提取 → 全局后缀.
- **执行安全**: 同目录 rename, 跨目录 copy (保护原件). 原地改名写入 sqlite 历史, 按 (目录, 校验和) 身份反查命名史.
- **可还原**: 原地改名记录 checksum 身份, 还原前重算校验和比对, 不一致即拒绝还原; copy 不落历史.
- **冲突检测**: 目标名重复 / 目标路径已存在均会标记冲突并阻止执行.

## 安装

```bash
# release build (~19 MB stripped)
cargo build --release
./target/release/subtitle-renamer
```

## 使用

1. 通过 Browse 按钮载入视频/字幕文件或文件夹(或在媒体目录下启动自动扫描当前目录).
2. 主界面呈现视频 | 字幕 | 目标文件名 三列视图.
3. 调整后缀(全局后缀, token→后缀映射), 预览即时刷新.
4. 检查未匹配项 / 冲突项,必要时手动解除或编辑.
5. 点击 Apply, 在确认对话框中确认后执行.
6. 历史面板可浏览过往会话并按单条 / 整批还原.

## 数据文件位置

- 历史数据库: `$XDG_DATA_HOME/subtitle-renamer/history.db` (`~/.local/share/subtitle-renamer/history.db`)
- 配置文件: `$XDG_CONFIG_HOME/subtitle-renamer/config.toml` (`~/.config/subtitle-renamer/config.toml`)

## 配置示例

```toml
custom_video_exts = []
custom_subtitle_exts = []
action_mode = "Auto"

[suffix]
global = ""
auto_extract_language_token = false

[suffix.token_map]
chs = "zh-Hans"
cht = "zh-Hant"

# 可选: 正则兜底(各含一个捕获组提取集数)
# video_regex = "(?i)ep(\\d+)"
# subtitle_regex = "(?i)ep(\\d+)"
```

## 开发

```bash
cargo fmt --check         # 格式检查
cargo clippy --all-targets -- -D warnings   # 严格 lint
cargo test                # 单元测试 + 语料集成测试
cargo build --release
```

模块划分:

```
src/
├── main.rs              # eframe 入口
├── lib.rs               # 公开 crate API
├── ui/                  # GUI 层 (依赖 eframe)
│   ├── app.rs           # eframe::App 实现: 三列表格, Browse 载入, 设置栏, 历史面板
│   └── fonts.rs         # CJK 字体注册(确保中文/日文界面可见)
└── core/                # GUI-free, 可独立单测
    ├── parse.rs         # 文件类型识别 + token 对齐 key 提取 + 归一化 (NFKD / whitespace)
    ├── matcher.rs       # 视频 × 字幕按 key 配对 + 手动指派 API
    ├── plan.rs          # 纯函数: 配对 + 后缀配置 → 操作列表 + 冲突列表
    ├── execute.rs       # rename / copy + 还原校验
    ├── history.rs       # sqlite 历史持久化层
    └── config.rs        # TOML 配置持久化
tests/
├── corpus.rs            # 语料驱动集成测试 (per-case pass/fail/wontfix 报告)
└── fixtures/
    ├── match_cases.json          # 9 个手写 case
    └── match_cases_subrenamer.json   # 12 个 case, 移植自 qwqcode/SubRenamer (GPL-2.0)
```

## AI 使用声明

本项目在开发过程中大量使用 AI 辅助生成代码. 作者仍在学习 Rust, 尚未对全部代码进行独立人工审阅, 使用前请自行评估风险.

## 许可

GPL-3.0-or-later. 详见 `LICENSE`. SubRenamer 语料来自 [qwqcode/SubRenamer](https://github.com/qwqcode/SubRenamer), 按 GPL-2.0 授权, 在 `tests/fixtures/match_cases_subrenamer.json` 文件头保留来源标注.
