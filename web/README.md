# mNest Web

基于 Vite、SolidJS、TypeScript 和 Tailwind CSS 的前端，包含播放器、标签刮削工作台和系统设置三个一级页面。

## 开发

```bash
npm install
npm run dev
```

Vite 开发服务器默认运行在 `http://127.0.0.1:5173`，并将 `/api`、`/user`、`/rest` 和 `/health` 代理到 `http://127.0.0.1:4535`。

## 检查与构建

```bash
npm run typecheck
npm test
npm run build
```

生产文件输出到 `web/dist`，由 Rust 服务直接托管。三套界面主题保存在浏览器 `localStorage`，认证使用后端签发的 HttpOnly Cookie。
