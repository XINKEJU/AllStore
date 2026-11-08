# MindosShop

一个纯净、轻量的原生 Windows 应用商店客户端。用联想应用商店的内容接口作为数据源，但**不含**广告、运营位、捆绑推荐、热搜推广词与遥测上报。

## 为什么是它

原版联想应用商店便携剪枝后仍有 235 MB / 5 个进程（CEF 浏览器壳），且在线 UI 由服务器下发，运营推广位无法从本地文件彻底移除。MindosShop 直接以合法客户端身份调用内容 API，自己绘制界面：

| | 原版（剪枝后） | MindosShop |
|---|---|---|
| 交付体积 | 234.9 MB | **258 KB（单 exe）** |
| 进程数 | 5 | **1** |
| 窗口 | CEF 浏览器壳 | **真 HWND** |
| 运行时 | CEF + 124 版 Chromium | **无（仅系统 DLL）** |
| 运营广告位 | 服务端下发，本地管不到 | **从不请求** |

## 技术架构

| 层 | 实现 |
|---|---|
| 窗口 | `CreateWindowExW` + 消息循环，DWM Mica / 沉浸式暗色，系统圆角与阴影 |
| 渲染 | Direct2D 硬件加速 + DirectWrite 系统字体 + WIC 解码图标 |
| 网络 | WinHTTP（复用系统 TLS 根证书与系统代理） |
| 数据 | serde_json，仅解析白名单内容接口 |
| 并发 | `std::thread` + `PostMessage`，无异步运行时 |
| 编译 | LTO + `opt-level="z"` + `panic="abort"` + `strip` |

不打包任何浏览器、.NET、VC++ 运行库或第三方 TLS 栈；只链接 Windows 系统自带 DLL。

## 纯净策略

**只请求这些接口**（`src/net.rs` 白名单）：

- `/appbase/token/register` — 注册客户端 token
- `/appstorecontents/class/all_classes_list` — 分类树
- `/appstorecontents/class/class_apps_list` — 分类应用列表
- `/searchcontent/search/v4.0/search_apps` — 搜索
- `/appstorecontents/app/details_new` — 应用详情
- 应用图标 CDN、安装包下载直链

**从不请求**：开屏广告、首页运营横幅、`op_recommend_contents`、`system/parameter`（捆绑推荐总闸）、`search_box`（付费热搜词）、`guess/likes`、AI 打点 `pc-fe-ai.lenovomm.cn`、遥测信标 `fsr.lenovomm.com`。搜索结果中 `isAdvert: true` 的条目在解析层直接丢弃。

## 身份与隐私

程序启动时以最小合法客户端画像（`ch` 版本 + OS 版本）向 `/appbase/token/register` 注册一个临时 token。**不采集、不上传**序列号、UUID、MAC、机型等任何设备标识，也不读取本机原版商店的缓存。已实测该身份可正常取得分类/搜索/详情数据。

## 功能

- 首页推荐、软件分类导航、关键词搜索（支持中文 IME）
- 应用卡片：官方图标、名称、简介、大小、安装量、评分
- 详情页：大图标、简介、版本信息
- 一键下载安装：WinHTTP 流式写盘到 `Downloads`，状态栏实时进度
- 深色/浅色跟随系统、DPI 感知、鼠标滚轮滚动、悬停高亮

## 构建

```powershell
# 需要 Rust（MSVC 工具链）与 Windows SDK
cargo build --release
# 产物：target\release\mindosshop.exe
```

GitHub Actions 会在 `windows-latest` 上构建并上传 `mindosshop-windows-x64` artifact；打 `v*` tag 时自动创建 Release。

## 安装

运行 `install.ps1`（或由 Actions 产物手动放置）：

```
%LOCALAPPDATA%\MindosShop\mindosshop.exe
```

同时创建开始菜单快捷方式与「添加或删除程序」卸载项。程序不写注册表配置、不驻留服务、不设开机自启。

## 说明

本项目为个人学习与本地使用而做。应用内容与商标归各自权利人所有；请遵守数据源的服务条款。仓库私有，不分发任何第三方安装包或凭据。
