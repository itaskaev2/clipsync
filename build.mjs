import { cpSync, mkdirSync, existsSync, rmSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const distDir = join(__dirname, 'dist');

if (existsSync(distDir)) {
  rmSync(distDir, { recursive: true });
}
mkdirSync(distDir, { recursive: true });

// Copy HTML, CSS, JS
for (const file of ['index.html', 'main.js', 'style.css']) {
  cpSync(join(__dirname, 'src', file), join(distDir, file));
}

console.log('Frontend built to dist/');
