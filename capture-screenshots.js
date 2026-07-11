// capture-screenshots.js
// Run: node capture-screenshots.js
// Captures the klayer dashboard overview screenshot in light theme with real data.

const { chromium } = require('playwright');
const path = require('path');

const BASE = 'http://localhost:7474';  // admin build — full real data
const OUT  = path.join(__dirname, 'docs', 'screenshot');
const W    = 1440;
const H    = 900;

// Pages to capture: [filename, nav-page-id, pre-capture setup fn (optional)]
const PAGES = [
  ['dashboard', 'overview', null],
];

(async () => {
  const browser = await chromium.launch({ headless: true });
  const ctx = await browser.newContext({
    viewport: { width: W, height: H },
    colorScheme: 'light',
    deviceScaleFactor: 2,
  });
  const page = await ctx.newPage();

  console.log('Loading dashboard...');
  await page.goto(BASE, { waitUntil: 'networkidle' });

  // Force light theme and English regardless of saved localStorage
  await page.evaluate(() => {
    document.body.classList.remove('dark');
    localStorage.setItem('klayer-theme', 'light');
    localStorage.setItem('klayer-lang', 'en');
  });

  // Wait for data to render (skeleton gone from stats)
  await page.waitForFunction(() => {
    const el = document.getElementById('s-domains');
    return el && el.textContent.trim() && !el.querySelector('.skeleton');
  }, { timeout: 10000 }).catch(() => console.warn('  stats not ready, continuing'));

  for (const [filename, pageId, setup] of PAGES) {
    console.log('  -> ' + filename + ' (' + pageId + ')');
    await page.evaluate((id) => {
      const btn = document.querySelector('[data-page="' + id + '"]');
      if (btn) btn.click();
    }, pageId);
    await page.waitForTimeout(500);

    if (setup) await setup(page);

    const dest = path.join(OUT, filename + '.png');
    await page.screenshot({ path: dest, clip: { x: 0, y: 0, width: W, height: H } });
    console.log('     saved ' + dest);

    // Restore G.isAdmin to default true for next pages
    await page.evaluate(() => {
      G.isAdmin = true;
    });
  }

  await browser.close();
  console.log('\nAll screenshots saved.');
})();
