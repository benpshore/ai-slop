import { copyFileSync, mkdirSync } from 'node:fs';
const root=new URL('../',import.meta.url);
mkdirSync(new URL('public/vendor/pdf-oxide',root),{recursive:true});
for(const file of ['pdf_oxide.js','pdf_oxide_bg.wasm'])copyFileSync(new URL(`node_modules/pdf-oxide-wasm/web/${file}`,root),new URL(`public/vendor/pdf-oxide/${file}`,root));
