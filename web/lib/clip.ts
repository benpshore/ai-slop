import { Readability } from '@mozilla/readability';
import DOMPurify from 'dompurify';
import TurndownService from 'turndown';
import { gfm } from 'turndown-plugin-gfm';
import type { Extracted, LinkEvidence } from './types';

export function safeUrl(value:string,base?:string){if(!value.trim())return null;try{const u=new URL(value,base);return ['http:','https:'].includes(u.protocol)&&!u.username&&!u.password?u.href:null;}catch{return null;}}
export function doiFrom(value:string){let s=value;try{s=decodeURIComponent(s);}catch{}return s.match(/10\.\d{4,9}\/[^\s<>"?#]+/i)?.[0].replace(/[.,;]+$/,'');}
export function textDois(text:string):LinkEvidence[]{return Array.from(new Set(text.match(/10\.\d{4,9}\/[^\s<>"?#]+/gi)||[])).map(doi=>{doi=doi.replace(/[.,;]+$/,'');return {url:`https://doi.org/${doi}`,doi,kind:'printed DOI'};}).slice(0,1000);}
export function clipHtml(source:string,url:string,title='Saved page'):Extracted{
 if(source.length>4*1024*1024)throw new Error('HTML exceeds the 4 MiB analysis limit.');
 const document=new DOMParser().parseFromString(source,'text/html');
 const base=safeUrl(document.querySelector('base[href]')?.getAttribute('href')||'',url)||url;
 const metadata:Record<string,unknown>={sourceUrl:url,capturedAt:new Date().toISOString(),capture:'HTML snapshot; scripts were not executed'};
 const metas:Record<string,string>={};for(const el of Array.from(document.querySelectorAll('meta[name],meta[property]')).slice(0,200)){const key=el.getAttribute('name')||el.getAttribute('property')||'';metas[key]=el.getAttribute('content')||'';}metadata.meta=metas;
 metadata.canonical=safeUrl(document.querySelector('link[rel=canonical]')?.getAttribute('href')||'',base);
 metadata.structuredData=Array.from(document.querySelectorAll('script[type="application/ld+json"]')).slice(0,20).map(el=>{try{return JSON.parse((el.textContent||'').slice(0,100000));}catch{return {unparsed:el.textContent?.slice(0,10000)};}});
 metadata.feeds=Array.from(document.querySelectorAll('link[rel=alternate]')).filter(el=>/rss|atom/.test(el.getAttribute('type')||'')).map(el=>({type:el.getAttribute('type'),url:safeUrl(el.getAttribute('href')||'',base)}));
 const links:LinkEvidence[]=Array.from(document.querySelectorAll('a[href]')).slice(0,4000).map(el=>{const raw=el.getAttribute('href')||'';let absolute=raw;try{absolute=new URL(raw,base).href;}catch{}return {url:absolute,label:(el.textContent||'').trim().slice(0,400),kind:'HTML link',doi:doiFrom(absolute)};});
 for(const el of Array.from(document.querySelectorAll('[href],[src]'))){for(const attr of ['href','src']){const value=el.getAttribute(attr);if(value){const absolute=safeUrl(value,base);if(absolute)el.setAttribute(attr,absolute);else el.removeAttribute(attr);}}}
 const tables=Array.from(document.querySelectorAll('table')).slice(0,100).map(t=>Array.from(t.querySelectorAll('tr')).slice(0,1000).map(row=>Array.from(row.querySelectorAll('th,td')).map(c=>(c.textContent||'').trim())));
 const images=Array.from(document.querySelectorAll('img')).slice(0,1000).map(el=>({url:el.getAttribute('src'),alt:el.getAttribute('alt')}));metadata.images=images;
 const warnings=['This is an HTML snapshot. Content added later by scripts, closed sections, canvas charts, and authenticated resources may be absent.'];
 const article=new Readability(document.cloneNode(true) as Document,{keepClasses:false,charThreshold:100}).parse();
 let html=article?.content||document.body.innerHTML;if(!article)warnings.push('Article detection did not find a main section; the full body was retained.');
 html=DOMPurify.sanitize(html,{FORBID_TAGS:['script','style','iframe','object','embed','form','input','button','svg','audio','video','source','track','picture','link'],FORBID_ATTR:['style','srcset']});
 const visible=new DOMParser().parseFromString(html,'text/html');
 for(const image of Array.from(visible.querySelectorAll('img'))){const label=visible.createElement('p');label.textContent=`[Image: ${image.getAttribute('alt')||'no description'}]`;image.replaceWith(label);}
 html=visible.body.innerHTML;
 const converter=new TurndownService({headingStyle:'atx',codeBlockStyle:'fenced'});converter.use(gfm);converter.keep(node=>node.nodeName.toLowerCase()==='math');const markdown=converter.turndown(html);
 const textTree=visible.body.cloneNode(true) as HTMLElement;
 for(const element of Array.from(textTree.querySelectorAll('p,div,section,article,h1,h2,h3,h4,h5,h6,li,tr,blockquote,pre,br'))){element.parentNode?.insertBefore(visible.createTextNode('\n'),element.nextSibling);}
 const text=(textTree.textContent||'').replace(/\n{3,}/g,'\n\n').trim();links.push(...textDois(text));
 return {title:(article?.title||document.title||title).slice(0,500),text,html,markdown,links:links.slice(0,5000),tables,metadata,warnings,engine:'Mozilla Readability + DOMPurify + GFM',status:'partial'};
}
export function parseFeed(source:string,url:string):Extracted{
 if(source.length>4*1024*1024||/<!DOCTYPE|<!ENTITY/i.test(source))throw new Error('Feed exceeds its limits or declares unsupported XML entities.');
 const xml=new DOMParser().parseFromString(source,'application/xml');if(xml.querySelector('parsererror'))throw new Error('The feed is not valid XML.');
 const root=xml.documentElement.localName;if(!['rss','feed','RDF'].includes(root))throw new Error('This is not an RSS or Atom feed.');
 const local=(el:Element,name:string)=>Array.from(el.children).find(x=>x.localName===name)?.textContent?.trim()||'';
 const nodes=Array.from(xml.getElementsByTagNameNS('*',root==='feed'?'entry':'item'));if(nodes.length>500)throw new Error('This alpha accepts at most 500 entries per feed.');
 const entries=nodes.map(el=>{
  const children=Array.from(el.children);const link=children.find(x=>x.localName==='link'&&(!x.getAttribute('rel')||x.getAttribute('rel')==='alternate'));
  const target=safeUrl(link?.getAttribute('href')||link?.textContent||'',url)||'';
  const contentElement=['encoded','content','description','summary'].map(name=>children.find(x=>x.localName===name)).find(Boolean);
  const content=contentElement?.getAttribute('type')==='xhtml'?contentElement.innerHTML:contentElement?.textContent||'';
  const clipped=clipHtml(content,target||url,local(el,'title'));
  return {id:local(el,'id')||local(el,'guid')||target||local(el,'title'),title:local(el,'title'),url:target,published:local(el,'published')||local(el,'pubDate'),updated:local(el,'updated'),author:local(el,'author')||local(el,'creator'),content:clipped.markdown,links:clipped.links,enclosures:children.filter(x=>x.localName==='enclosure'||(x.localName==='link'&&x.getAttribute('rel')==='enclosure')).map(x=>({url:safeUrl(x.getAttribute('url')||x.getAttribute('href')||'',url),type:x.getAttribute('type'),length:x.getAttribute('length')}))};
 });
 const container=root==='feed'?xml.documentElement:xml.querySelector('channel')||xml.documentElement;
 const links:LinkEvidence[]=entries.flatMap(entry=>[{url:entry.url,label:entry.title,kind:'feed entry'},...entry.links,...entry.enclosures.filter(e=>e.url).map(e=>({url:e.url!,kind:'enclosure',label:e.type||'Attachment'}))]).filter(l=>l.url).slice(0,5000);
 const markdown=entries.map(e=>`## ${e.title}\n\n${e.url}\n\n${e.content}`).join('\n\n');
 return {title:local(container,'title')||'Feed',text:markdown,markdown,links,entries,warnings:['Feed content may be a summary. Linked articles and enclosures are not fetched automatically.'],metadata:{sourceUrl:url,capturedAt:new Date().toISOString(),format:root,entryCount:entries.length},engine:'RSS 2.0 / Atom parser',status:'partial'};
}
