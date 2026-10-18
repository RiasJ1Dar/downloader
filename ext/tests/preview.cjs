// Локальний UI-стенд: лише два явно дозволені файли, без доступу до repo.
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const files = new Map([
  ['/content-fixture.html', [path.join(__dirname, 'content-fixture.html'), 'text/html']],
  ['/content.js', [path.join(__dirname, '../chrome/content.js'), 'text/javascript']],
]);
http.createServer((req, res) => {
  const file = files.get(new URL(req.url, 'http://localhost').pathname);
  if (!file) { res.writeHead(404); res.end(); return; }
  res.setHeader('Content-Type', `${file[1]}; charset=utf-8`);
  fs.createReadStream(file[0]).on('error', () => { res.destroy(); }).pipe(res);
}).listen(8878, '127.0.0.1', () => console.log('http://127.0.0.1:8878/content-fixture.html'));
