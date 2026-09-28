// examples/serve.js — 手动测试页静态服务器（仅本地开发用）
// 用法: node examples/serve.js [端口，默认 8099]
const http = require("http");
const fs = require("fs");
const path = require("path");

const root = path.resolve(__dirname, "..");
const port = Number(process.argv[2]) || 8099;
const types = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript",
  ".mjs": "text/javascript",
  ".css": "text/css",
  ".json": "application/json",
};

const server = http.createServer((req, res) => {
  let p = path.join(root, decodeURIComponent(req.url.split("?")[0]));
  if (p.endsWith("/") || p.endsWith("\\")) p = path.join(p, "index.html");
  fs.readFile(p, (err, data) => {
    if (err) {
      res.statusCode = 404;
      res.end("not found");
      return;
    }
    res.setHeader("Content-Type", types[path.extname(p).toLowerCase()] || "application/octet-stream");
    res.end(data);
  });
});

server.on("error", (e) => {
  console.error(`[serve] 端口 ${port} 被占用（可能上次的 serve.js 还在跑）: ${e.code}`);
  process.exit(1);
});

server.listen(port, "127.0.0.1", () => {
  console.log(`[serve] 测试页: http://127.0.0.1:${port}/examples/page/index.html`);
});
