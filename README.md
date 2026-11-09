# MindosShop

一个纯净、轻量的原生 Windows 应用商店客户端。用联想应用商店的内容接口作为数据源，但**不含**广告、运营位、捆绑推荐、热搜推广词与遥测上报。

当前版本：**0.3.0**

## 为什么是它

原版联想应用商店便携剪枝后仍有 235 MB / 5 个进程（CEF 浏览器壳），且在线 UI 由服务器下发，运营推广位无法从本地文件彻底移除。MindosShop 直接以合法客户端身份调用内容 API，自己绘制界面：

| | 原版（剪枝后） | MindosShop |
|---|---|---|
| 交付体积 | 234.9 MB | **448.5 KB（单 exe，静态 CRT）** |
| 进程数 | 5 | **1** |
| 窗口 | CEF 浏览器壳 | **真 HWND** |
| 运行时 | CEF + 124 版 Chromium | **无（仅系统 DLL）** |
| 运营广告位 | 服务端下发，本地管不到 | **从不请求** |

## 技术架构

| 层 | 实现 |
|---|---|
| 窗口 | `CreateWindowExW` + 消息循环，DWM Mica / 沉浸式暗色，最小窗口尺寸约束 |
| 渲染 | Direct2D 硬件加速 + DirectWrite 系统字体 + WIC 解码图标；画刷与字体格式缓存、图标预缩放、内容区轴对齐裁剪 |
| 网络 | WinHTTP（复用系统 TLS 根证书与系统代理），显式处理 HTTP 状态码与重定向 |
| 数据 | serde_json，仅解析白名单内容接口 |
| 更新 | 读取卸载项注册表 + **本地**比对商店目录版本（`installed.rs` / `update.rs`） |
| 并发 | `std::thread` + `PostMessage`，请求带序号以丢弃过期响应 |
| 编译 | LTO + `opt-level="z"` + `panic="abort"` + `strip` + 静态 CRT（无需 VC++ 运行库） |

不打包任何浏览器、.NET、VC++ 运行库或第三方 TLS 栈；只链接 Windows 系统自带 DLL。

## 下载完整性（0.3.0 修复）

商店的 `/dlservice/download?softid=...` 会返回 **302 跳转到明文 HTTP 的 CDN**。WinHTTP 默认禁止 HTTPS→HTTP 降级，旧版既不检查状态码也不读 `Location`，于是读到 0 字节并报「下载内容为空」；软件更新页的「全部更新」同样全军覆没。

现在 `net.rs` 显式接管重定向（最多 6 跳，支持相对路径与协议降级），并做完整校验：

- 非 2xx 直接报错并显示状态码，不再把 404/500 错误页存成 `.exe`
- 重定向后的请求**不携带 token 等身份头**，避免泄漏给第三方 CDN
- 用 `Content-Length` 核对实际写入字节数，读数据出错立即失败而不是当作完成
- 先写 `.part` 临时文件，校验通过才重命名；失败自动删除残包
- 按最终 CDN 文件名选择真实扩展名（`.exe` / `.msi` / `.zip` / `.7z` / `.apk`）
- 校验文件头（PE 的 `MZ`、MSI 的 OLE 头、ZIP 的 `PK`），拦截网关劫持页
- 同名文件自动加序号，不覆盖已有下载

实测：跟随 302 后成功下载 1,136,096 字节的真实安装包，文件头为 `MZ`，临时文件全部清理。

## 界面（0.3.0 重排）

- 统一的间距、字号与按钮尺寸；顶栏 / 导航 / 内容 / 状态栏四区固定，内容区轴对齐裁剪，**文字不会再盖到顶栏或按钮上**
- 应用卡片：名称、两行简介、大小与安装量、安装按钮各自独立分区，超长文字以省略号收尾
- 详情页：修复 `developer` 为对象导致详情反序列化失败、长简介始终回退成一行短介绍的问题；现在显示完整简介、本次更新、版本 / 大小 / 安装量 / 开发者，且**正文支持滚轮滚动**并带滚动条
- 渲染层修复 `center_text` 复用共享字体格式导致后续文字错位；画刷改为按调色板缓存；超大图标先缩放到 128 px 再上传显存
- 悬停与点击共用一套命中判定，导航、按钮、卡片反馈一致；快速切换分类时旧响应会被丢弃

## 纯净策略

**只请求这些接口**（`src/net.rs` 白名单）：

- `/appbase/token/register` — 注册客户端 token
- `/appstorecontents/class/all_classes_list` — 分类树
- `/appstorecontents/class/class_apps_list` — 分类应用列表
- `/searchcontent/search/v4.0/search_apps` — 搜索
- `/appstorecontents/app/details_new` — 应用详情
- 应用图标 CDN、安装包下载直链

**从不请求**：开屏广告、首页运营横幅、`op_recommend_contents`、`system/parameter`（捆绑推荐总闸）、`search_box`（付费热搜词）、`guess/likes`、AI 打点 `pc-fe-ai.lenovomm.cn`、遥测信标 `fsr.lenovomm.com`。搜索结果中 `isAdvert: true` 的条目在解析层直接丢弃。更新检查复用上面的分类/列表接口，不新增任何接口。

## 身份与隐私

程序启动时以最小合法客户端画像（`ch` 版本 + OS 版本）向 `/appbase/token/register` 注册一个临时 token。**不采集、不上传**序列号、UUID、MAC、机型等任何设备标识，也不读取本机原版商店的缓存。

**软件更新是纯本地比对**：程序读取本机卸载项注册表得到「名称 + 版本」，再从商店目录取回「名称 + 版本」，两者只在本机内存中匹配；**本机装了什么软件从不发送给服务器**。检查结果缓存于 `%LOCALAPPDATA%\MindosShop\updates.json`，可随时删除。本机实测扫描到 151 个已安装应用并匹配出 9 个可更新项。

本地诊断日志写在 `%TEMP%\mindosshop.log`，超过 1 MB 自动轮转；**不记录响应体或搜索关键词**。

## 功能

- 应用列表、软件分类导航、关键词搜索（支持中文 IME）
- 应用卡片：官方图标、名称、简介、大小、安装量
- 详情页：大图标、完整简介、本次更新、版本 / 大小 / 安装量 / 开发者、可滚动正文
- 下载安装：WinHTTP 流式写盘到 `Downloads`，状态栏实时进度，临时包与签名校验
- **软件更新**：左侧「软件更新」页，支持检查更新 / 单个更新 / 全部更新 / 忽略；版本号分段比较（忽略构建标记误报）、同名取最新版本、中英文名称归一化匹配、结果缓存
- 深色/浅色跟随系统、DPI 感知、滚轮滚动（列表 / 详情 / 更新页均有上限）、悬停高亮

> 安装与更新都只下载安装包，**不会自动运行安装程序**；请在 `Downloads` 中自行运行下载的文件完成安装。更新覆盖度取决于商店目录能否拉到对应软件，名称匹配为启发式，极端命名可能漏报。

## 构建

```powershell
# 需要 Rust（MSVC 工具链，edition 2024 需 >= 1.85）与 Windows SDK
cargo build --release
# 产物：target\release\mindosshop.exe

cargo test                        # 16 个单元测试（含布局几何、版本比较、解析回归）
cargo test -- --ignored --nocapture  # 4 个联网冒烟测试（真实 302 / 真实下载 / 更新匹配）
cargo clippy --all-targets        # 零警告
cargo fmt --check
```

联网冒烟测试默认忽略，其中「真实下载」会向 `Downloads` 写入约 1 MB 的自检安装包，校验通过后自动删除。

GitHub Actions 会在 `windows-latest` 上构建并上传 `mindosshop-windows-x64` artifact；打 `v*` tag 时自动创建 Release。

## 安装

解压发布包后双击 `Install.cmd`（或直接运行 `mindosshop.exe` 免安装使用）。安装为**用户级、无需管理员**：

```
%LOCALAPPDATA%\MindosShop\mindosshop.exe
```

同时创建开始菜单快捷方式与「添加或删除程序」卸载项。程序不写注册表配置、不驻留服务、不设开机自启；仅把更新检查缓存写入 `%LOCALAPPDATA%\MindosShop\updates.json`。exe 未签名，首次运行若被 SmartScreen 拦截，点「更多信息 → 仍要运行」。

## 说明

本项目为个人学习与本地使用而做。应用内容与商标归各自权利人所有；请遵守数据源的服务条款。仓库私有，不分发任何第三方安装包或凭据。
