# summon 控制台(前端)

Lit 3 + Vite + TypeScript。构建产物 `dist/` 入库,Rust 在编译期嵌进 `summon` 二进制,
所以 `cargo build` 不需要 Node。前后端唯一契约是 `src/api/types.ts`。

```bash
npm install          # 依赖版本锁在 package-lock.json
npm run dev          # 开发服务器,跑内存里的示例数据(src/api/mock.ts),所有页面可改可存
npm test             # tsc --noEmit + stylelint + vitest + 体积预算(读 dist/)
npm run build        # 生成 dist/,改了前端源码后重新构建并提交
npm run check:dist   # 重新构建一次,确认入库的 dist/ 与源码一致
npm run screenshots  # 示例环境下各页面截图(需要本机 Chrome),默认写到 tasks/20261020-dashboard-console/screens
```

示例环境里左下角「示例数据」菜单可以模拟别处改了文件(演示冲突)、会话失效、切换身份;
地址带 `?as=owner` 以本机身份打开,带 `?set=key=value` 直接改整机这一层。
