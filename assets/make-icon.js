const fs = require('fs');
const { Resvg } = require('@resvg/resvg-js');

// Sparkle: tapered rays of alternating length, slightly irregular (hand-drawn feel).
function sparkle(cx, cy, R, rays = 12) {
  let out = '';
  for (let i = 0; i < rays; i++) {
    const a = (i / rays) * Math.PI * 2 + (i % 3) * 0.035 - Math.PI / 2;
    const len = R * (i % 2 ? 0.8 : 1) * (1 - (i % 4) * 0.03);
    const base = R * 0.16, r0 = R * 0.12, tip = R * 0.07;
    const px = Math.cos(a + Math.PI / 2), py = Math.sin(a + Math.PI / 2);
    const bx = cx + Math.cos(a) * r0, by = cy + Math.sin(a) * r0;
    const tx = cx + Math.cos(a) * len, ty = cy + Math.sin(a) * len;
    out += `<path d="M ${bx + px * base} ${by + py * base} L ${tx + px * tip} ${ty + py * tip} A ${tip} ${tip} 0 0 0 ${tx - px * tip} ${ty - py * tip} L ${bx - px * base} ${by - py * base} Z"/>`;
  }
  return `<g fill="url(#spark)">${out}<circle cx="${cx}" cy="${cy}" r="${R * 0.2}"/></g>`;
}

const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#34302c"/><stop offset="1" stop-color="#1a1816"/>
    </linearGradient>
    <linearGradient id="spark" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#f0a07f"/><stop offset="1" stop-color="#d0643f"/>
    </linearGradient>
    <linearGradient id="r" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#fbf7f0"/><stop offset="1" stop-color="#e6dfd3"/>
    </linearGradient>
    <filter id="shadow" x="-20%" y="-20%" width="140%" height="140%">
      <feDropShadow dx="0" dy="12" stdDeviation="14" flood-color="#000" flood-opacity="0.35"/>
    </filter>
  </defs>
  <!-- macOS icon grid: 824px body centred in 1024 with ~185px corner radius -->
  <rect x="100" y="100" width="824" height="824" rx="185" fill="url(#bg)" filter="url(#shadow)"/>
  <rect x="101.5" y="101.5" width="821" height="821" rx="183.5" fill="none" stroke="#ffffff" stroke-opacity="0.09" stroke-width="3"/>
  <text x="452" y="752" text-anchor="middle" font-family="Avenir Next" font-weight="800" font-size="560" fill="url(#r)" letter-spacing="-10">R</text>
  ${sparkle(718, 306, 150, 10)}
</svg>`;

fs.writeFileSync('icon.svg', svg);
const png = new Resvg(svg, { font: { loadSystemFonts: true, defaultFontFamily: 'Helvetica Neue' }, fitTo: { mode: 'width', value: 1024 } }).render().asPng();
fs.writeFileSync('icon.png', png);
const small = new Resvg(svg, { font: { loadSystemFonts: true }, fitTo: { mode: 'width', value: 64 } }).render().asPng();
fs.writeFileSync('icon64.png', small);
console.log('ok');
