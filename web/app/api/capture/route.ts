import { owner, boundedBody, failure } from '@/lib/server';

function publicIp(ip:string){
 if(ip.includes(':'))return !/^(::|fc|fd|fe[89ab]|ff|2001:db8)/i.test(ip)&&!ip.toLowerCase().includes('ffff:');
 const [a,b]=ip.split('.').map(Number);return Number.isFinite(a)&&![0,10,127].includes(a)&&a<224&&!(a===169&&b===254)&&!(a===172&&b>=16&&b<=31)&&!(a===192&&b===168)&&!(a===100&&b>=64&&b<=127)&&!(a===198&&(b===18||b===19));
}
async function allowed(value:string){
 const u=new URL(value);if(!['https:','http:'].includes(u.protocol)||u.username||u.password||(u.port&&!['80','443'].includes(u.port))||!u.hostname.includes('.')||/[:\[\]]/.test(u.hostname)||/^(\d+\.){3}\d+$/.test(u.hostname)||/(localhost|\.local|\.internal|\.localhost|\.test|\.invalid|\.chatgpt\.site|\.workers\.dev)$/i.test(u.hostname))throw new Error('Use a public HTTP or HTTPS page URL.');
 const answers=await Promise.all(['A','AAAA'].map(async type=>{const response=await fetch(`https://cloudflare-dns.com/dns-query?name=${encodeURIComponent(u.hostname)}&type=${type}`,{headers:{Accept:'application/dns-json'},signal:AbortSignal.timeout(6000)});if(!response.ok)throw new Error('Could not verify the destination.');const data=await response.json() as {Answer?:{type:number,data:string}[]};return (data.Answer||[]).filter(a=>[1,28].includes(a.type)).map(a=>a.data);}));
 const ips=answers.flat();if(!ips.length||ips.some(ip=>!publicIp(ip)))throw new Error('The destination is not a public web server.');return u;
}
export async function POST(request:Request){try{
 await owner(request);const payload=JSON.parse(new TextDecoder().decode(await boundedBody(request,8192)));if(typeof payload.url!=='string')throw new Error('A URL is required.');
 let current=payload.url;
 for(let redirects=0;redirects<5;redirects++){
  const url=await allowed(current);const response=await fetch(url,{redirect:'manual',headers:{Accept:'text/html,application/xhtml+xml,application/rss+xml,application/atom+xml,application/xml,text/xml','User-Agent':'TPE-Private-Alpha/1.0'},signal:AbortSignal.timeout(12000)});
  if(response.status>=300&&response.status<400){const location=response.headers.get('location');await response.body?.cancel();if(!location)throw new Error('The page returned an empty redirect.');current=new URL(location,url).href;continue;}
  if(!response.ok){await response.body?.cancel();throw new Error(`The source returned HTTP ${response.status}. You can import a saved HTML snapshot instead.`);}
  const type=response.headers.get('content-type')||'';if(!/html|xml|rss|atom|text\/plain/i.test(type)){await response.body?.cancel();throw new Error('This URL is not an HTML page or feed. Download PDFs and use file import.');}
  const bytes=await boundedBody(response,4*1024*1024);
  const prefix=new TextDecoder('ascii').decode(bytes.slice(0,2048));
  const encoding=type.match(/charset\s*=\s*["']?([^\s;"']+)/i)?.[1]||prefix.match(/(?:charset|encoding)\s*=\s*["']?([^\s;"'/>]+)/i)?.[1]||'utf-8';
  let decoder:TextDecoder;try{decoder=new TextDecoder(encoding);}catch{decoder=new TextDecoder();}
  let binary='';for(let offset=0;offset<bytes.length;offset+=16384)binary+=String.fromCharCode(...bytes.subarray(offset,offset+16384));
  return Response.json({html:decoder.decode(bytes),originalBase64:btoa(binary),url:url.href,contentType:type},{headers:{'Cache-Control':'private, no-store'}});
 }
 throw new Error('The page redirected too many times.');
}catch(e){return failure(e);}}
