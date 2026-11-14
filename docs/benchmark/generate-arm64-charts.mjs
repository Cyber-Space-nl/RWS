// Generate the README charts from the checked-in aggregate benchmark data.
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const dir = dirname(fileURLToPath(import.meta.url));
const data = JSON.parse(readFileSync(join(dir, 'arm64-2026-11-13.json'), 'utf8'));
const servers = [
  { id: 'rws', label: 'RWS', color: '#80d5d7' },
  { id: 'nginx', label: 'Nginx', color: '#92b5d5' },
  { id: 'apache', label: 'Apache', color: '#c6afd9' },
];
if (!data.complete || data.runs !== 36) throw new Error('Expected complete 36-run summary');

function chart(metric, max, digits, suffix, title, description, filename) {
  let groups = '';
  for (const [index, file, name] of [[0, 'small', '67-byte file'], [1, '4k', '4 KiB file']]) {
    const x = 24 + index * 448;
    groups += `<text x="${x}" y="88" class="group">${name}</text>`;
    for (const [row, server] of servers.entries()) {
      const matches = data.results.filter(r => r.server === server.id && r.file === file && r.rate === 600);
      if (matches.length !== 1 || matches[0].n !== 3 || (metric === 'cpu' && matches[0].monitor_n !== 2)) {
        throw new Error(`Unexpected measurement count for ${server.id}/${file}`);
      }
      const value = matches[0][metric];
      if (!Number.isFinite(value) || value < 0 || value > max) throw new Error('Invalid measurement');
      const y = 110 + row * 35;
      groups += `<text x="${x}" y="${y + 11}" class="label">${server.label}</text>`;
      groups += `<rect x="${x + 74}" y="${y}" width="255" height="12" rx="2" fill="#26353d"/>`;
      groups += `<rect x="${x + 74}" y="${y}" width="${(255 * value / max).toFixed(2)}" height="12" rx="2" fill="${server.color}"/>`;
      groups += `<text x="${x + 418}" y="${y + 11}" text-anchor="end" class="value">${value.toFixed(digits)}${suffix}</text>`;
    }
  }
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="920" height="242" viewBox="0 0 920 242" role="img" aria-labelledby="title desc">
<title id="title">${title}</title><desc id="desc">${description}</desc>
<style>text{font-family:system-ui,-apple-system,Segoe UI,sans-serif}.title{font-size:19px;font-weight:700;fill:#edf4f5}.subtitle{font-size:12px;fill:#b7c8cd}.group{font-size:13px;font-weight:600;fill:#e3edef}.label,.value{font-size:13px;fill:#e3edef}.value{font-family:ui-monospace,monospace}</style>
<rect width="920" height="242" rx="7" fill="#101a20"/><text x="24" y="35" class="title">${title}</text><text x="24" y="57" class="subtitle">${description}</text>${groups}</svg>\n`;
  writeFileSync(join(dir, filename), svg);
}
chart('p95', 5, 2, ' ms', 'p95 response latency', 'Median of three runs · 600 requests/s offered · includes SSH tunnel', 'arm64-latency.svg');
chart('cpu', 10, 1, '%', 'Server CPU', 'Median of two monitored runs · percent of one vCPU · 600 requests/s offered', 'arm64-cpu.svg');
console.log('Generated ARM64 benchmark charts');
