# harness-desktop

### DeepSeek Harness 的桌面，功能极简，只保留必要功能，无额外性能消耗，

启动应用时会自动执行 `npx --yes @deepseek-ai/dsh web --host 127.0.0.1 --port 3080 --no-open`（无控制台窗口、不自动打开浏览器），
服务就绪后主窗口通过 `<iframe>` 嵌入 `http://127.0.0.1:3080`（不导航离开壳子页）。
退出应用时 dsh 进程树由 Windows Job Object（KILL_ON_JOB_CLOSE）保证一并终止——即使被任务管理器强杀也不会残留。

## 功能

- 自动启动 dsh 本地服务（无需手动输入命令）
- 启动中显示状态页；服务就绪自动通过 iframe 嵌入页面
- 端口 3080 若已被 **同款 harness** 占用则直接复用（不重复启动）
- **右下角圆形浮动图标**（DOM 实现）：主窗口右下角齿轮按钮，点击后**窗口中央弹出配置弹窗**：
  服务状态 / 重启服务 / 在浏览器中打开 / 开机自启动开关 / 退出
- **开机自启动**：配置弹窗内开关或托盘菜单勾选「开机自启动」即注册为登录自启
  （Windows 写入 `HKCU\...\CurrentVersion\Run`，两处状态自动同步）
- **系统托盘**：关闭窗口驻留托盘（不退出），左键点击托盘图标显示主窗口；
  托盘菜单：显示主窗口 / **升级 dsh** / 重启服务 / 在浏览器中打开 / 开机自启动 / 退出
- **升级 dsh**：托盘菜单手动触发——停止服务、清除 npx 缓存中的 `@deepseek-ai/dsh`、
  重启服务并自动下载最新版（首次可能需 1-2 分钟，状态页显示进度）
- 服务崩溃/超时后状态页与弹窗提示，可一键重启
- 日志在 `%LOCALAPPDATA%\com.deepseek.harness-desktop\logs\dsh-server.log`

## 开发

```bash
npm install          # 安装前端依赖 + tauri cli
npm run tauri dev    # 开发模式（需要 rust 工具链）
```

## 打包

```bash
npm run tauri build
```

产物在 `src-tauri/target/release/`（exe 与 NSIS/MSI 安装包）。

## 环境要求

- Windows 10/11（需 WebView2 Runtime，系统一般自带）
- Node.js（`npx` 可用）—— dsh 服务依赖
- 首次启动时 npx 会下载 `@deepseek-ai/dsh` 并安装 profile 依赖，可能需要 1–3 分钟
