import { copyFileSync, mkdirSync } from 'node:fs';
mkdirSync('public/vendor/pdf-oxide',{recursive:true});
for(const file of ['pdf_oxide.js','pdf_oxide_bg.wasm'])copyFileSync(`node_modules/pdf-oxide-wasm/web/${file}`,`public/vendor/pdf-oxide/${file}`);
