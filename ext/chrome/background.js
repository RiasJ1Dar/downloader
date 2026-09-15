// MV3: downloads.onCreated + cancel, далі native host. Не localhost-порт.
const HOST = "com.downloader.host";

function sendToHost(payload) {
  chrome.runtime.sendNativeMessage(HOST, payload, (resp) => {
    if (chrome.runtime.lastError) {
      console.warn("Downloader host:", chrome.runtime.lastError.message);
      return;
    }
    if (resp && resp.ok) {
      console.info("Downloader завдання", resp.id);
    } else {
      console.warn("Downloader:", resp && resp.error);
    }
  });
}

function withCookies(url, referer, extraUrl) {
  const payload = { url: extraUrl || url, referer: referer || url };
  try {
    const u = new URL(payload.url);
    chrome.cookies.getAll({ url: u.origin }, (list) => {
      if (list && list.length) {
        payload.cookies = list.map((c) => c.name + "=" + c.value).join("; ");
      }
      sendToHost(payload);
    });
  } catch (_) {
    sendToHost(payload);
  }
}

const lastManifest = new Map();

// 1. Пасивний мережевий спостерігач для маніфестів (ловить fetch, XHR і web workers)
function recordManifest(d) {
  const u = d.url || "";
  if (!/\.m3u8(\?|$)/i.test(u) && !/\.mpd(\?|$)/i.test(u)) return;
  if (d.tabId >= 0) lastManifest.set(d.tabId, u);
}

if (chrome.webRequest && chrome.webRequest.onBeforeRequest) {
  chrome.webRequest.onBeforeRequest.addListener(
    recordManifest,
    { urls: ["http://*/*", "https://*/*"] }
  );
} else if (chrome.webRequest && chrome.webRequest.onCompleted) {
  chrome.webRequest.onCompleted.addListener(
    recordManifest,
    { urls: ["http://*/*", "https://*/*"] }
  );
}

// 2. Очищення кешу при закритті вкладки
chrome.tabs.onRemoved.addListener((tabId) => {
  lastManifest.delete(tabId);
});

// 3. Обробка повідомлень від сніффера та кнопки над відео
chrome.runtime.onMessage.addListener((msg, sender) => {
  if (!msg) return;
  const tabId = sender.tab && sender.tab.id;

  if (msg.op === "manifest_detected" && msg.url && tabId >= 0) {
    lastManifest.set(tabId, msg.url);
    return;
  }

  if (msg.op !== "add") return;
  const manifest = tabId >= 0 ? lastManifest.get(tabId) : undefined;
  const target = manifest || msg.media || msg.page;
  if (!target) return;
  withCookies(msg.page || target, msg.page, target);
});

// 4. Контекстні меню
chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: "dl-link",
      title: chrome.i18n.getMessage("ctxLink"),
      contexts: ["link"],
    });
    chrome.contextMenus.create({
      id: "dl-page",
      title: chrome.i18n.getMessage("ctxPage"),
      contexts: ["page"],
    });
    chrome.contextMenus.create({
      id: "dl-media",
      title: chrome.i18n.getMessage("ctxMedia"),
      contexts: ["video", "audio"],
    });
  });
});

chrome.contextMenus.onClicked.addListener((info, tab) => {
  const page = (tab && tab.url) || info.pageUrl || "";
  let target = page;
  if (info.menuItemId === "dl-link" && info.linkUrl) target = info.linkUrl;
  if (info.menuItemId === "dl-media" && info.srcUrl) target = info.srcUrl;
  if (!target.startsWith("http://") && !target.startsWith("https://")) return;
  withCookies(page || target, page, target);
});

// 5. Перехоплення завантажень: downloads.onCreated -> cancel -> native messaging
chrome.downloads.onCreated.addListener((item) => {
  const url = item.url || "";
  if (!url.startsWith("http://") && !url.startsWith("https://")) return;
  if (item.byExtensionId === chrome.runtime.id) return;

  chrome.downloads.cancel(item.id, () => {
    chrome.downloads.erase({ id: item.id });
  });

  const payload = {
    url,
    referer: item.referrer || undefined,
  };

  const send = (cookies) => {
    if (cookies) payload.cookies = cookies;
    chrome.runtime.sendNativeMessage(HOST, payload, (resp) => {
      if (chrome.runtime.lastError) {
        console.warn("Downloader host:", chrome.runtime.lastError.message);
        return;
      }
      if (resp && resp.ok) {
        console.info("Downloader завдання", resp.id);
      } else {
        console.warn("Downloader:", resp && resp.error);
      }
    });
  };

  try {
    const u = new URL(url);
    chrome.cookies.getAll({ url: u.origin }, (list) => {
      if (!list || !list.length) {
        send(undefined);
        return;
      }
      const header = list.map((c) => c.name + "=" + c.value).join("; ");
      send(header);
    });
  } catch (_) {
    send(undefined);
  }
});
