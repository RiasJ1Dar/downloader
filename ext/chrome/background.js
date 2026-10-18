// MV3: pause → підтверджене створення завдання → cancel/erase.
const HOST = "com.downloader.host";
const HOST_TIMEOUT = 10000;
const MEDIA_TTL = 30 * 60 * 1000;
const manifests = new Map();
const requests = new Map();
const handoffs = new Map();

function httpUrl(value) {
  try {
    const url = new URL(value);
    return ["http:", "https:"].includes(url.protocol) ? url.href : undefined;
  } catch { return undefined; }
}

function apiCall(object, name, ...args) {
  return new Promise((resolve, reject) => {
    try {
      object[name](...args, (result) => {
        const error = chrome.runtime.lastError;
        if (error) reject(new Error(error.message));
        else resolve(result);
      });
    } catch (error) { reject(error); }
  });
}

function accepted(response) {
  return response?.ok === true && Number.isSafeInteger(response.id) && response.id > 0;
}

function sendToHost(payload, onLateAccepted) {
  return new Promise((resolve) => {
    let settled = false;
    let acceptedAtFinish = false;
    let lateReconciled = false;
    const finish = (result) => {
      if (settled) return;
      settled = true;
      acceptedAtFinish = accepted(result);
      clearTimeout(timer);
      resolve(result);
    };
    const timer = setTimeout(() => finish({ ok: false, error: "Ядро Downloader не підтвердило передачу вчасно. Перевірте, чи воно запущене." }), HOST_TIMEOUT);
    try {
      chrome.runtime.sendNativeMessage(HOST, payload, (response) => {
        const error = chrome.runtime.lastError;
        if (settled) {
          // Пізнє підтвердження не повинне лишити два активні завантаження.
          if (!acceptedAtFinish && !lateReconciled && !error && accepted(response) && onLateAccepted) {
            lateReconciled = true;
            onLateAccepted().catch((e) => console.warn("Downloader: late handoff:", e.message));
          }
          return;
        }
        finish(error ? { ok: false, error: error.message } :
          accepted(response) ? response : { ok: false, error: response?.error || "Ядро Downloader не підтвердило створення завдання" });
      });
    } catch (error) { finish({ ok: false, error: error.message }); }
  });
}

function pruneRequests() {
  const now = Date.now();
  for (const [id, r] of requests) if (now - r.at > 30000) requests.delete(id);
  while (requests.size > 256) requests.delete(requests.keys().next().value);
}

function rememberRequest(d) {
  pruneRequests();
  const old = requests.get(d.requestId);
  requests.set(d.requestId, { ...old, url: d.url, method: d.method || "GET",
    tabId: d.tabId, frameId: d.frameId || 0, documentId: d.documentId,
    at: Date.now(), incognito: !!d.incognito, headers: d.requestHeaders || old?.headers || [] });
}

function requestFor(url, tabId, frameId, incognito) {
  pruneRequests();
  return [...requests.values()].filter((r) => r.url === url &&
    (tabId == null || r.tabId === tabId) && (frameId == null || r.frameId === frameId) &&
    (incognito == null || r.incognito === incognito))
    .sort((a, b) => b.at - a.at)[0];
}

function forgetFrame(tabId, frameId) {
  for (const [key, value] of manifests) {
    if (value.tabId === tabId && (frameId == null || value.frameId === frameId)) manifests.delete(key);
  }
  for (const [key, value] of requests) {
    if (value.tabId === tabId && (frameId == null || value.frameId === frameId)) requests.delete(key);
  }
}

function recordManifest(d, type) {
  const url = httpUrl(d.url);
  if (!url || d.tabId < 0) return;
  const kind = type || (/\.m3u8(?:[?#]|$)/i.test(url) ? "HLS" : /\.mpd(?:[?#]|$)/i.test(url) ? "DASH" : undefined);
  if (!kind) return;
  const frameId = d.frameId || 0;
  const key = `${d.tabId}:${frameId}:${url}`;
  manifests.set(key, { url, type: kind, tabId: d.tabId, frameId,
    documentId: d.documentId, documentUrl: d.documentUrl, at: Date.now() });
  for (const [id, value] of manifests) if (Date.now() - value.at > MEDIA_TTL) manifests.delete(id);
  while (manifests.size > 256) manifests.delete(manifests.keys().next().value);
}

function mediaCandidates(sender) {
  return [...manifests.values()].filter((r) => r.tabId === sender.tab?.id &&
    r.frameId === (sender.frameId || 0) && Date.now() - r.at <= MEDIA_TTL &&
    (!r.documentId || !sender.documentId || r.documentId === sender.documentId) &&
    (!r.documentUrl || !sender.url || r.documentUrl === sender.url))
    .map(({ url, type }) => ({ url, type }));
}

async function cookieHeader(url, frame = {}) {
  const details = { url }; // Повний шлях: origin губив cookies із Path=/private/.
  if (frame.tabId >= 0 && chrome.cookies.getAllCookieStores) {
    const stores = await apiCall(chrome.cookies, "getAllCookieStores");
    const store = stores.find((s) => s.tabIds.includes(frame.tabId));
    if (!store) throw new Error("Не вдалося визначити сховище cookies цієї вкладки");
    details.storeId = store.id;
  } else if (frame.incognito) {
    throw new Error("Не вдалося безпечно визначити приватне сховище cookies; завантаження лишається браузеру");
  }
  const plain = await apiCall(chrome.cookies, "getAll", details);
  let partitioned = [];
  if (frame.tabId >= 0 && chrome.cookies.getPartitionKey) {
    const result = await apiCall(chrome.cookies, "getPartitionKey", { tabId: frame.tabId, frameId: frame.frameId || 0 });
    if (result?.partitionKey?.topLevelSite) {
      partitioned = await apiCall(chrome.cookies, "getAll", { ...details, partitionKey: result.partitionKey });
    }
  }
  return [...(plain || []), ...(partitioned || [])]
    .sort((a, b) => (b.path?.length || 0) - (a.path?.length || 0))
    .map((c) => `${c.name}=${c.value}`).join("; ") || undefined;
}

async function payloadFor(url, referer, frame = {}) {
  const request = requestFor(url, frame.tabId, frame.frameId, frame.incognito);
  if (request && request.method !== "GET") throw new Error("Цей запит треба залишити браузеру: метод не GET");
  const header = (name) => request?.headers.find((h) => h.name.toLowerCase() === name)?.value;
  return { url, referer, cookies: header("cookie") || await cookieHeader(url, frame),
    context: { version: 1, source_url: url, page_url: referer, method: "GET",
      user_agent: header("user-agent") || globalThis.navigator?.userAgent,
      headers: (request?.headers || []).filter((h) => ["accept", "accept-language", "origin"].includes(h.name.toLowerCase()))
        .map((h) => [h.name, h.value]) } };
}

// Після перезапуску MV3 worker не лишає браузерні завантаження на паузі.
async function recoverPending() {
  try {
    const all = await apiCall(chrome.storage.session, "get", null);
    for (const [key, value] of Object.entries(all || {})) {
      if (!key.startsWith("handoff:")) continue;
      try {
        if (value.accepted) await completeHandoff(value.id);
        else {
          const [item] = await apiCall(chrome.downloads, "search", { id: value.id });
          if (item?.state === "in_progress" && item.paused) await apiCall(chrome.downloads, "resume", value.id);
        }
        await apiCall(chrome.storage.session, "remove", key);
      } catch (error) { console.warn("Downloader: незавершена передача лишилася для повтору:", error.message); }
    }
  } catch (error) { console.warn("Downloader: handoff recovery:", error.message); }
}
const recovery = recoverPending();

async function completeHandoff(id) {
  const [item] = await apiCall(chrome.downloads, "search", { id });
  if (item?.state !== "in_progress") return; // Не стираємо вже готовий файл/історію.
  await apiCall(chrome.downloads, "cancel", id);
  await apiCall(chrome.downloads, "erase", { id });
}

async function acceptHandoff(id) {
  const key = `handoff:${id}`;
  await apiCall(chrome.storage.session, "set", { [key]: { id, accepted: true } });
  await completeHandoff(id);
  await apiCall(chrome.storage.session, "remove", key);
}

async function preparePayload(url, referer, frame) {
  let timer;
  try {
    return await Promise.race([payloadFor(url, referer, frame), new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error("Не вдалося вчасно одержати контекст браузера")), HOST_TIMEOUT);
    })]);
  } finally { clearTimeout(timer); }
}

async function intercept(item) {
  const url = httpUrl(item.finalUrl || item.url);
  if (!url || item.paused || (item.state && item.state !== "in_progress") ||
    item.byExtensionId === chrome.runtime.id || handoffs.has(item.id)) return;
  const request = requestFor(url, undefined, undefined, item.incognito);
  if (request && request.method !== "GET") return;
  const frame = { tabId: request?.tabId, frameId: request?.frameId, incognito: item.incognito };
  handoffs.set(item.id, true);
  const key = `handoff:${item.id}`;
  let paused = false;
  let coreAccepted = false;
  let keepPending = false;
  try {
    await recovery;
    await apiCall(chrome.storage.session, "set", { [key]: { id: item.id } });
    await apiCall(chrome.downloads, "pause", item.id);
    paused = true;
    const response = await sendToHost(await preparePayload(url, item.referrer, frame), () => acceptHandoff(item.id));
    if (!accepted(response)) throw new Error(response.error);
    coreAccepted = true;
    await acceptHandoff(item.id);
  } catch (error) {
    console.warn("Downloader: download remains in browser:", error.message);
    if (coreAccepted) keepPending = true;
    else if (paused) {
      try { await apiCall(chrome.downloads, "resume", item.id); }
      catch (resumeError) { keepPending = true; console.warn("Downloader: не вдалося поновити браузерне завантаження:", resumeError.message); }
    }
  } finally {
    try { if (!keepPending) await apiCall(chrome.storage.session, "remove", key); }
    catch (error) { console.warn("Downloader: handoff cleanup:", error.message); }
    handoffs.delete(item.id);
  }
}

chrome.webRequest.onBeforeRequest.addListener((d) => {
  if (d.type === "main_frame") forgetFrame(d.tabId);
  else if (d.type === "sub_frame") forgetFrame(d.tabId, d.frameId);
  rememberRequest(d); recordManifest(d);
}, { urls: ["http://*/*", "https://*/*"] });
try {
  chrome.webRequest.onBeforeSendHeaders.addListener(rememberRequest,
    { urls: ["http://*/*", "https://*/*"] }, ["requestHeaders", "extraHeaders"]);
} catch (error) {
  // Firefox не приймає Chromium-специфічний extraHeaders.
  console.debug("Downloader: extraHeaders недоступний:", error.message);
  chrome.webRequest.onBeforeSendHeaders.addListener(rememberRequest,
    { urls: ["http://*/*", "https://*/*"] }, ["requestHeaders"]);
}
chrome.webRequest.onHeadersReceived.addListener((d) => {
  const mime = d.responseHeaders?.find((h) => h.name.toLowerCase() === "content-type")?.value || "";
  recordManifest(d, /mpegurl/i.test(mime) ? "HLS" : /dash\+xml/i.test(mime) ? "DASH" : undefined);
}, { urls: ["http://*/*", "https://*/*"] }, ["responseHeaders"]);
chrome.tabs.onRemoved.addListener((id) => forgetFrame(id));
chrome.tabs.onUpdated.addListener((id, change) => {
  if (change.url || change.status === "loading") forgetFrame(id);
});

chrome.runtime.onMessage.addListener((msg, sender, respond) => {
  if (!msg) return;
  if (msg.op === "manifest_detected") {
    recordManifest({ url: msg.url, tabId: sender.tab?.id ?? -1, frameId: sender.frameId,
      documentId: sender.documentId, documentUrl: sender.url });
    respond({ ok: true }); return;
  }
  if (msg.op !== "add") return;
  (async () => {
    const candidates = mediaCandidates(sender);
    if (msg.choice && !candidates.some((c) => c.url === msg.choice)) throw new Error("Обраний медіапотік застарів. Оновіть сторінку й оберіть знову.");
    const target = httpUrl(msg.choice) || httpUrl(msg.media) ||
      (candidates.length === 1 ? candidates[0].url : undefined);
    if (!target && candidates.length > 1) return { ok: false, candidates };
    const url = target || httpUrl(sender.url || msg.page);
    if (!url) throw new Error("Не знайдено посилання для завантаження");
    return sendToHost(await preparePayload(url, sender.url || msg.page,
      { tabId: sender.tab?.id, frameId: sender.frameId, incognito: sender.tab?.incognito }));
  })().then(respond, (error) => respond({ ok: false, error: error.message }));
  return true;
});

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    for (const [id, text, contexts] of [["dl-link", "ctxLink", ["link"]],
      ["dl-page", "ctxPage", ["page"]], ["dl-media", "ctxMedia", ["video", "audio"]]]) {
      chrome.contextMenus.create({ id, title: chrome.i18n.getMessage(text), contexts });
    }
  });
});
chrome.contextMenus.onClicked.addListener((info, tab) => {
  const page = info.frameUrl || tab?.url || info.pageUrl;
  const url = httpUrl(info.menuItemId === "dl-link" ? info.linkUrl :
    info.menuItemId === "dl-media" ? info.srcUrl : page);
  if (!url) return;
  preparePayload(url, page, { tabId: tab?.id, frameId: info.frameId, incognito: tab?.incognito })
    .then((payload) => sendToHost(payload)).then((r) => { if (!r.ok) console.warn("Downloader:", r.error); })
    .catch((error) => console.warn("Downloader:", error.message));
});
chrome.downloads.onCreated.addListener((item) => { void intercept(item); });
