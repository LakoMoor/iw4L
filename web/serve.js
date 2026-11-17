// Local dev server: static files with the headers WebAssembly threads need.
const http = require("http"), fs = require("fs"), path = require("path");
const root = path.resolve(process.argv[2] || "."), extra = process.argv[3] ? path.resolve(process.argv[3]) : null;
const types = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm", ".css": "text/css", ".iw4pk": "application/octet-stream" };
http.createServer((req, res) => {
  let url = decodeURIComponent(req.url.split("?")[0]);
  let base = root;
  if (extra && url.startsWith("/packout/")) { base = extra; url = url.slice("/packout".length); }
  const file = path.join(base, url === "/" ? "index.html" : url);
  if (!file.startsWith(base)) { res.writeHead(403); return res.end(); }
  fs.stat(file, (err, st) => {
    if (err || !st.isFile()) { res.writeHead(404); return res.end("not found"); }
    const headers = {
      "Content-Type": types[path.extname(file)] || "application/octet-stream",
      "Accept-Ranges": "bytes",
      "Cross-Origin-Opener-Policy": "same-origin",
      "Cross-Origin-Embedder-Policy": "require-corp",
      "Cross-Origin-Resource-Policy": "cross-origin",
      "Cache-Control": "no-store",
    };
    const m = /bytes=([0-9]+)-([0-9]*)/.exec(req.headers.range || "");
    if (m) {
      const a = Number(m[1]), b = m[2] ? Number(m[2]) : st.size - 1;
      res.writeHead(206, { ...headers, "Content-Range": `bytes ${a}-${b}/${st.size}`, "Content-Length": b - a + 1 });
      return fs.createReadStream(file, { start: a, end: b }).pipe(res);
    }
    res.writeHead(200, { ...headers, "Content-Length": st.size });
    fs.createReadStream(file).pipe(res);
  });
}).listen(8080, () => console.log("http://localhost:8080/"));
