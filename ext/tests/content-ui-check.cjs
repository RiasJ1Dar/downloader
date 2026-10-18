// Окремий локальний smoke test: використовує наданий Playwright, не нову залежність проєкту.
const { chromium } = require(process.argv[2]);
const assert = require('node:assert/strict');
const path = require('node:path');
const fs = require('node:fs');
(async () => {
  const browser = await chromium.launch({ executablePath: process.argv[3], headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1024, height: 768 } });
    const errors = []; page.on('pageerror', e => errors.push(e.message));
    // Лише в тестовому browser context відкриваємо shadow для локаторів.
    await page.addInitScript(() => {
      const original = Element.prototype.attachShadow;
      Element.prototype.attachShadow = function (init) { return original.call(this, { ...init, mode: 'open' }); };
    });
    const base = 'http://127.0.0.1:8878/content-fixture.html';
    const screenshots = process.argv[4]; fs.mkdirSync(screenshots, { recursive: true });
    await page.goto(base);
    const button = page.getByRole('button', { name: 'Завантажити', exact: true });
    await button.click(); await page.locator('select').waitFor({ state: 'visible' });
    assert.equal(await page.locator('select option').count(), 3);
    await page.locator('select').selectOption({ index: 2 });
    await button.click(); await page.getByRole('status').filter({ hasText: 'Додано в Downloader #42' }).waitFor();
    assert.equal(await button.isEnabled(), true);
    await page.screenshot({ path: path.join(screenshots, 'media-added-desktop.png') });
    await page.setViewportSize({ width: 375, height: 667 });
    await page.goto(base); await button.click(); await page.locator('select').waitFor({ state: 'visible' });
    const bounds = await page.locator('select').boundingBox();
    assert.ok(bounds.x >= 8 && bounds.x + bounds.width <= 367);
    await page.screenshot({ path: path.join(screenshots, 'media-choice-narrow.png') });
    await page.locator('select').press('Escape');
    assert.equal(await page.locator('select').isVisible(), false);
    assert.equal(await button.evaluate(el => el.getRootNode().activeElement === el), true);
    await page.goto(base + '?error=1'); await button.click();
    await page.getByRole('status').filter({ hasText: 'Ядро не запущене' }).waitFor();
    assert.equal(await button.isEnabled(), true);
    await page.screenshot({ path: path.join(screenshots, 'media-error-narrow.png') });
    assert.deepEqual(errors, []);
    console.log('UI: додавання, явний вибір, Escape/focus, помилка й 375px — OK');
  } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
