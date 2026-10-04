function publicIp(ip:string){
 if(ip.includes(':'))return !/^(::|fc|fd|fe[89ab]|ff|2001:db8)/i.test(ip)&&!ip.toLowerCase().includes('ffff:');
 const [a,b]=ip.split('.').map(Number);return Number.isFinite(a)&&![0,10,127].includes(a)&&a<224&&!(a===169&&b===254)&&!(a===172&&b>=16&&b<=31)&&!(a===192&&b===168)&&!(a===100&&b>=64&&b<=127)&&!(a===198&&(b===18||b===19));
}
export async function allowed(value:string){
 const u=new URL(value);if(!['https:','http:'].includes(u.protocol)||u.username||u.password||(u.port&&!['80','443'].includes(u.port))||!u.hostname.includes('.')||/[:\[\]]/.test(u.hostname)||/^(\d+\.){3}\d+$/.test(u.hostname)||/(localhost|\.local|\.internal|\.localhost|\.test|\.invalid|\.chatgpt\.site|\.workers\.dev)$/i.test(u.hostname))throw new Error('Use a public HTTP or HTTPS page URL.');
 const answers=await Promise.all(['A','AAAA'].map(async type=>{const response=await fetch(`https://cloudflare-dns.com/dns-query?name=${encodeURIComponent(u.hostname)}&type=${type}`,{headers:{Accept:'application/dns-json'},signal:AbortSignal.timeout(6000)});if(!response.ok)throw new Error('Could not verify the destination.');const data=await response.json() as {Answer?:{type:number,data:string}[]};return (data.Answer||[]).filter(a=>[1,28].includes(a.type)).map(a=>a.data);}));
 const ips=answers.flat();if(!ips.length||ips.some(ip=>!publicIp(ip)))throw new Error('The destination is not a public web server.');return u;
}

export async function fetchPublicSource(value:string,accept:string,signal?:AbortSignal){
 let current=value;
 for(let redirects=0;redirects<5;redirects++){
  const url=await allowed(current);
  const response=await fetch(url,{redirect:'manual',headers:{Accept:accept,'User-Agent':'TPE-Private-Alpha/1.0'},signal});
  if(response.status>=300&&response.status<400){const location=response.headers.get('location');await response.body?.cancel();if(!location)throw new Error('The source returned an empty redirect.');current=new URL(location,url).href;continue;}
  if(!response.ok){await response.body?.cancel();throw new Error(`The source returned HTTP ${response.status}.`);}
  return {response,url:url.href};
 }
 throw new Error('The source redirected too many times.');
}
