const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const source = fs.readFileSync(path.join(__dirname, '../chrome/background.js'), 'utf8');
const flush = async () => { for (let i = 0; i < 5; i++) await new Promise(setImmediate); };

function fixture(saved = {}) {
  const calls = [], native = [], timers = new Map(), storage = { ...saved };
  const items = new Map([[9, { id: 9, state: 'in_progress', paused: false }]]);
  const event = () => ({ listeners: [], addListener(fn) { this.listeners.push(fn); } });
  const chrome = {
    runtime: { id: 'test-extension', onMessage: event(), onInstalled: event(),
      sendNativeMessage(host, payload, callback) { native.push({ host, payload, callback }); } },
    downloads: { onCreated: event(),
      pause(id, cb) { calls.push('pause'); items.get(id).paused = true; cb(); },
      resume(id, cb) { calls.push('resume'); items.get(id).paused = false; cb(); },
      cancel(id, cb) { calls.push('cancel'); items.get(id).state = 'interrupted'; cb(); },
      erase(q, cb) { calls.push('erase'); cb([q.id]); },
      search(q, cb) { cb(items.has(q.id) ? [items.get(q.id)] : []); } },
    storage: { session: {
      get(q, cb) { cb({ ...storage }); }, set(values, cb) { Object.assign(storage, values); cb(); },
      remove(key, cb) { delete storage[key]; cb(); } } },
    cookies: {
      getAll(q, cb) { calls.push({ cookies: q }); cb([{ name: 'session', value: 'path-token', path: '/private/' }]); },
      getAllCookieStores(cb) { cb([{ id: 'private-store', tabIds: [7] }]); } },
    tabs: { onRemoved: event(), onUpdated: event() },
    webRequest: { onBeforeRequest: event(), onBeforeSendHeaders: event(), onHeadersReceived: event() },
    contextMenus: { onClicked: event(), removeAll(cb) { cb(); }, create() {} },
    i18n: { getMessage(key) { return key; } },
  };
  let timerId = 0;
  const context = vm.createContext({ chrome, URL, console: { warn() {}, debug() {} },
    navigator: { userAgent: 'Browser-Test/1' },
    setTimeout(fn) { timers.set(++timerId, fn); return timerId; }, clearTimeout(id) { timers.delete(id); } });
  vm.runInContext(source, context);
  const api = vm.runInContext('({intercept,cookieHeader,recordManifest,mediaCandidates,rememberRequest})', context);
  const message = (msg, sender = { tab: { id: 7 }, frameId: 0, documentId: 'doc', url: 'https://site.test/page' }) =>
    new Promise((resolve) => chrome.runtime.onMessage.listeners[0](msg, sender, resolve));
  return { chrome, api, calls, native, timers, storage, items, message,
    item: { id: 9, url: 'https://site.test/private/file.zip', incognito: false } };
}

test('браузер скасовується лише після підтвердженого завдання', async () => {
  const f = fixture(); await flush();
  const work = f.api.intercept(f.item); await flush();
  assert.deepEqual(f.calls.filter(x => typeof x === 'string'), ['pause']);
  assert.equal(f.native.length, 1);
  f.native[0].callback({ ok: true, id: 42 }); await work;
  assert.deepEqual(f.calls.filter(x => typeof x === 'string'), ['pause', 'cancel', 'erase']);
});

test('відсутній host, відмова та неправильне підтвердження відновлюють браузер', async () => {
  for (const reply of [undefined, { ok: false, error: 'no' }, { ok: true }, { ok: true, id: -1 }]) {
    const f = fixture(); await flush(); const work = f.api.intercept(f.item); await flush();
    if (!reply) f.chrome.runtime.lastError = { message: 'host missing' };
    f.native[0].callback(reply); delete f.chrome.runtime.lastError; await work;
    assert.deepEqual(f.calls.filter(x => typeof x === 'string'), ['pause', 'resume']);
    assert.equal(Object.keys(f.storage).length, 0);
  }
});

test('timeout відновлює браузер, пізнє підтвердження узгоджує власника', async () => {
  const f = fixture(); await flush(); const work = f.api.intercept(f.item); await flush();
  [...f.timers.values()][0](); await work;
  assert.deepEqual(f.calls.filter(x => typeof x === 'string'), ['pause', 'resume']);
  f.native[0].callback({ ok: true, id: 42 }); await flush();
  assert.deepEqual(f.calls.filter(x => typeof x === 'string'), ['pause', 'resume', 'cancel', 'erase']);
});

test('повторна подія й повторний callback не дублюють передачу', async () => {
  const f = fixture(); await flush(); const work = f.api.intercept(f.item);
  await f.api.intercept(f.item); await flush(); assert.equal(f.native.length, 1);
  f.native[0].callback({ ok: true, id: 42 }); await work;
  f.native[0].callback({ ok: true, id: 42 }); await flush();
  assert.equal(f.calls.filter(x => x === 'cancel').length, 1);
  assert.equal(f.calls.filter(x => x === 'erase').length, 1);
});

test('помилка pause не запускає host і не скасовує download', async () => {
  const f = fixture(); await flush();
  f.chrome.downloads.pause = (id, cb) => {
    f.chrome.runtime.lastError = { message: 'already complete' }; cb(); delete f.chrome.runtime.lastError;
  };
  await f.api.intercept(f.item);
  assert.equal(f.native.length, 0); assert.ok(!f.calls.includes('cancel'));
});

test('GET-контекст зберігає User-Agent, а POST лишається браузеру', async () => {
  const f = fixture(); await flush();
  f.api.rememberRequest({ requestId: 'post', url: f.item.url, method: 'POST', tabId: 7 });
  await f.api.intercept(f.item); assert.equal(f.native.length, 0); assert.ok(!f.calls.includes('pause'));
  f.api.rememberRequest({ requestId: 'post', url: f.item.url, method: 'GET', tabId: 7,
    requestHeaders: [{ name: 'User-Agent', value: 'Actual-UA' }, { name: 'Authorization', value: 'secret' }] });
  const work = f.api.intercept(f.item); await flush();
  assert.equal(f.native[0].payload.context.user_agent, 'Actual-UA');
  assert.equal(f.native[0].payload.context.headers.length, 0);
  f.native[0].callback({ ok: true, id: 42 }); await work;
});

test('cookies мають повний URL, правильний store і partition key', async () => {
  const f = fixture(); await flush();
  f.chrome.cookies.getPartitionKey = (q, cb) => cb({ partitionKey: { topLevelSite: 'https://site.test' } });
  await f.api.cookieHeader(f.item.url, { tabId: 7, frameId: 2 });
  const queries = f.calls.filter(x => x.cookies).map(x => x.cookies);
  assert.equal(queries.length, 2);
  for (const q of queries) { assert.equal(q.url, f.item.url); assert.equal(q.storeId, 'private-store'); }
  assert.equal(queries[1].partitionKey.topLevelSite, 'https://site.test');
});

test('прямий MP4 не підміняється старим manifest', async () => {
  const f = fixture(); await flush();
  f.api.recordManifest({ tabId: 7, frameId: 0, documentId: 'doc', url: 'https://site.test/old.m3u8' });
  const response = f.message({ op: 'add', media: 'https://site.test/new.mp4' }); await flush();
  assert.equal(f.native[0].payload.url, 'https://site.test/new.mp4');
  f.native[0].callback({ ok: true, id: 42 }); assert.equal((await response).ok, true);
});

test('кадри й документи ізольовані, кілька manifest потребують вибору', async () => {
  const f = fixture(); await flush();
  for (const [frameId, documentId, file] of [[0, 'doc', 'a.m3u8'], [0, 'doc', 'b.mpd'], [1, 'iframe', 'c.m3u8']])
    f.api.recordManifest({ tabId: 7, frameId, documentId, url: `https://site.test/${file}` });
  const response = await f.message({ op: 'add' });
  assert.equal(response.candidates.length, 2); assert.equal(f.native.length, 0);
  assert.equal(f.api.mediaCandidates({ tab: { id: 7 }, frameId: 0, documentId: 'new' }).length, 0);
  f.chrome.tabs.onUpdated.listeners[0](7, { url: 'https://site.test/new-page' });
  assert.equal(f.api.mediaCandidates({ tab: { id: 7 }, frameId: 0 }).length, 0);
});

test('перезапуск worker відновлює незавершену паузу, але не чіпає готовий файл', async () => {
  const f = fixture({ 'handoff:9': { id: 9 } }); f.items.get(9).paused = true; await flush();
  assert.ok(f.calls.includes('resume')); assert.equal(Object.keys(f.storage).length, 0);
  const g = fixture({ 'handoff:9': { id: 9 } }); g.items.get(9).state = 'complete'; await flush();
  assert.ok(!g.calls.includes('resume')); assert.ok(!g.calls.includes('erase'));
});

test('підтверджена передача після рестарту не запускає браузерну копію', async () => {
  const f = fixture({ 'handoff:9': { id: 9, accepted: true } }); await flush();
  assert.deepEqual(f.calls.filter(x => typeof x === 'string'), ['cancel', 'erase']);
});

test('помилка erase після підтвердження не відновлює вже переданий download', async () => {
  const f = fixture(); await flush();
  f.chrome.downloads.erase = (q, cb) => {
    f.chrome.runtime.lastError = { message: 'temporary failure' }; cb(); delete f.chrome.runtime.lastError;
  };
  const work = f.api.intercept(f.item); await flush(); f.native[0].callback({ ok: true, id: 42 }); await work;
  assert.ok(!f.calls.includes('resume')); assert.equal(f.storage['handoff:9'].accepted, true);
});

test('помилка resume лишає запис для відновлення worker', async () => {
  const f = fixture(); await flush();
  f.chrome.downloads.resume = (id, cb) => {
    f.chrome.runtime.lastError = { message: 'temporary failure' }; cb(); delete f.chrome.runtime.lastError;
  };
  const work = f.api.intercept(f.item); await flush(); f.native[0].callback({ ok: false }); await work;
  assert.equal(f.storage['handoff:9'].id, 9);
});

test('завислі cookies не лишають браузер на паузі й не запускають host пізніше', async () => {
  const f = fixture(); await flush(); let cookiesCallback;
  f.chrome.cookies.getAll = (q, cb) => { cookiesCallback = cb; };
  const work = f.api.intercept(f.item); await flush();
  [...f.timers.values()][0](); await work;
  assert.ok(f.calls.includes('resume')); assert.equal(f.native.length, 0);
  cookiesCallback([]); await flush(); assert.equal(f.native.length, 0);
});

test('приватний запит не підмішує cookies звичайному завантаженню', async () => {
  const f = fixture(); await flush();
  f.api.rememberRequest({ requestId: 'private', url: f.item.url, method: 'GET', incognito: true, tabId: 7,
    requestHeaders: [{ name: 'Cookie', value: 'private=secret' }] });
  const work = f.api.intercept(f.item); await flush();
  assert.equal(f.native[0].payload.cookies, 'session=path-token');
  f.native[0].callback({ ok: true, id: 42 }); await work;
});
