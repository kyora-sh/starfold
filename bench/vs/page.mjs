// Renders bench/scroll.html to a PNG: real text, the same pixels in every arm. Usage: node page.mjs <out.png>
import { createRequire } from 'module';
const require = createRequire(process.env.PLAYWRIGHT_NODE_MODULES);
const { chromium } = require('playwright-core');
const b = await chromium.launch({ executablePath: process.env.CHROMIUM, args: ['--no-sandbox'], headless: true });
const p = await b.newPage({ viewport: { width: 1920, height: 1080 } });
await p.goto(`file://${new URL(process.env.PAGE || '../scroll.html', import.meta.url).pathname}`);
await p.waitForTimeout(500);
await p.screenshot({ path: process.argv[2], clip: { x: 0, y: 0, width: 1920, height: 7080 }, fullPage: true });
await b.close();
