import { createReadStream } from "node:fs";
import { stat } from "node:fs/promises";
import { createServer } from "node:http";
import { extname, join, resolve, sep } from "node:path";

const root = resolve("out");
const basePath = "/omnisession";
const port = 4173;
const contentTypes = {
  ".css": "text/css; charset=utf-8",
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".png": "image/png",
  ".svg": "image/svg+xml",
  ".txt": "text/plain; charset=utf-8",
  ".woff2": "font/woff2",
};

createServer(async (request, response) => {
  const url = new URL(request.url ?? "/", `http://${request.headers.host}`);
  if (url.pathname === basePath) {
    response.writeHead(308, { location: `${basePath}/` });
    response.end();
    return;
  }
  if (!url.pathname.startsWith(`${basePath}/`)) {
    response.writeHead(404).end();
    return;
  }

  const relativePath = decodeURIComponent(url.pathname.slice(basePath.length + 1));
  let filePath = resolve(root, relativePath || "index.html");
  if (filePath !== root && !filePath.startsWith(`${root}${sep}`)) {
    response.writeHead(403).end();
    return;
  }

  try {
    const fileStat = await stat(filePath);
    if (fileStat.isDirectory()) {
      filePath = join(filePath, "index.html");
      await stat(filePath);
    }
    response.setHeader("content-type", contentTypes[extname(filePath)] ?? "application/octet-stream");
    createReadStream(filePath).pipe(response);
  } catch {
    response.writeHead(404).end();
  }
}).listen(port, "127.0.0.1");
