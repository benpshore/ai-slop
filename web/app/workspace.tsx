'use client';

import {useCallback, useEffect, useMemo, useRef, useState} from 'react';
import type {ClipboardEvent, DragEvent} from 'react';
import {AlertCircle, ArrowUp, Check, ChevronDown, Copy, Download, FileText, FolderOpen, ImagePlus, LoaderCircle, LockKeyhole, Search, Upload, X} from 'lucide-react';
import DOMPurify from 'dompurify';
import {marked} from 'marked';
import {Button} from '@/components/ui/button';
import {ClearCachedFilesButton, DeleteStoredDocumentButton, ImportBatchProgress, ImportItemControls} from '@/components/import-controls';
import {clipHtml, parseFeed, safeUrl, textDois, doiFrom} from '@/lib/clip';
import {expandUploads} from '@/lib/imports';
import {recognizeImage} from '@/lib/image-ocr';
import {extractOffice} from '@/lib/office';
import {captureSource, saveExtracted, uploadOriginal, uploadAssetFile, decodeSource} from '@/lib/upload-client';
import {retainArticleImages} from '@/lib/article-assets';
import {clearSavedWorkspaceCache, readWorkspace, writeWorkspace} from '@/lib/workspace-storage';
import {deleteStoredDocument, StoredDocumentDeletionError} from '@/lib/document-client';
import {cancelImportItem, filesFromDrop, ImportAttemptRegistry, restoreImportItems, retryImportItem, selectedImportFiles} from '@/lib/import-queue';
import type {ImportAttempt} from '@/lib/import-queue';
import {prepareTextImport} from '@/lib/text-import';
import type {DocumentRow, Extracted} from '@/lib/types';

type Phase = 'waiting'|'fetching'|'uploading'|'extracting'|'saving'|'saved'|'failed'|'cancelled'|'interrupted';
type Source = {type:'file';file:File;url?:string;decoded?:string;member?:boolean}|{type:'url';url:string;feed:boolean}|{type:'stored';name:string;url?:string;decoded?:string;member?:boolean};
type QueueItem = {id:string;name:string;source:Source;phase:Phase;progress:number|null;message:string;error?:string;record?:DocumentRow;result?:Extracted;savePending?:boolean;retrySave?:boolean;parentId?:string};
type Selection = {queueId:string}|{record:DocumentRow;result:Extracted|null};
type ReadingMode = 'reading'|'plain';
type PendingDeletion = {record:DocumentRow;cleanupComplete:boolean};
type Snapshot = {version:1;items:QueueItem[];draft:{url:string;kind:string;paste:string;query:string};selection:{queueId?:string;documentId?:string}|null;view:string;readingMode?:ReadingMode;scroll:number;pendingDeletion?:PendingDeletion};
const activePhases = new Set<Phase>(['waiting','fetching','uploading','extracting','saving']);
const messageOf = (error:unknown) => error instanceof Error ? error.message : String(error);
const phaseLabel = (phase:Phase) => ({waiting:'Waiting',fetching:'Fetching',uploading:'Saving original',extracting:'Extracting',saving:'Saving result',saved:'Saved',failed:'Needs attention',cancelled:'Cancelled',interrupted:'Interrupted'})[phase];
const progressValue = (item:QueueItem) => item.progress!==null&&Number.isFinite(item.progress)?Math.min(100,Math.max(0,item.progress)):undefined;
function StageProgress({item}:{item:QueueItem}) {
  const value=progressValue(item);
  return <div className="stage-progress"><progress max={100} value={value} aria-label={phaseLabel(item.phase)+' for '+item.name} aria-valuetext={item.message+(value===undefined?'':' '+Math.round(value)+'% of this stage.')}/><p className="help">{item.message}{value!==undefined&&<span className="stage-percent">{Math.round(value)}% of this stage</span>}</p></div>;
}

async function json<T>(response:Response):Promise<T> {
  if (!response.ok) { let message=await response.text();try { message=JSON.parse(message).error||message; } catch {} throw new Error(message||'Request failed ('+response.status+').'); }
  return response.json() as Promise<T>;
}
function download(name:string,value:string,type='application/json') {
  const url=URL.createObjectURL(new Blob([value],{type}));const anchor=document.createElement('a');anchor.href=url;anchor.download=name;anchor.click();setTimeout(()=>URL.revokeObjectURL(url),5000);
}
function pdf(bytes:ArrayBuffer,name:string,signal:AbortSignal,onProgress:(completed:number,total:number)=>void):Promise<Extracted> {
  return new Promise((resolve,reject)=>{
    signal.throwIfAborted();const worker=new Worker('/pdf-worker.js',{type:'module'});
    const stop=()=>{worker.terminate();signal.removeEventListener('abort',abort);};
    const abort=()=>{stop();reject(new DOMException('Extraction cancelled.','AbortError'));};
    signal.addEventListener('abort',abort,{once:true});
    worker.onmessage=event=>{const data=event.data;if(data.progress!==undefined)onProgress(data.progress,data.total);else if(data.error){stop();reject(new Error(data.error));}else if(data.result){stop();const result=data.result as Extracted;result.links.push(...textDois(result.text));resolve(result);}};
    worker.onerror=event=>{stop();reject(new Error(event.message||'The PDF worker stopped unexpectedly.'));};
    try { worker.postMessage({bytes,name},[bytes]); } catch(error) {stop();reject(error);}
  });
}
function nativeRecord(text:string,name:string):Extracted {
  let parsed;try {parsed=JSON.parse(text);}catch {parsed=text.split(/\r?\n/).filter(Boolean).map(line=>JSON.parse(line));}
  const records=Array.isArray(parsed)?parsed:[parsed];
  if(records.length!==1||!Array.isArray(records[0]?.pages)){const formatted=JSON.stringify(parsed,null,2);return {title:name,text:formatted,markdown:formatted,links:textDois(formatted),warnings:[],metadata:{format:'JSON'},engine:'JSON decoder',status:'ready'};}
  const entry=records[0];
  const pages=entry.pages as {text?:string;page?:number;links?:{uri?:string;bbox?:unknown}[]}[];
  return {title:typeof entry.metadata?.title==='string'?entry.metadata.title:name,text:pages.map(page=>typeof page.text==='string'?page.text:'').join('\n\n'),pages,links:pages.flatMap(page=>(Array.isArray(page.links)?page.links:[]).filter(link=>typeof link.uri==='string').map(link=>({url:link.uri!,rect:link.bbox,page:page.page,kind:'imported PDF link'}))),warnings:[...(Array.isArray(entry.warnings)?entry.warnings.filter((value:unknown)=>typeof value==='string'):[]),'Imported native output: its engine and status have not been independently verified here.'],metadata:{nativeRecord:entry},engine:'Imported: '+(entry.backend?.name||'TPE')+' '+(entry.backend?.version||''),status:'partial'};
}

async function supportsOcr(file:File):Promise<boolean> {const bytes=new Uint8Array(await file.slice(0,12).arrayBuffer());return (bytes[0]===0x89&&bytes[1]===0x50&&bytes[2]===0x4e&&bytes[3]===0x47)||(bytes[0]===0xff&&bytes[1]===0xd8&&bytes[2]===0xff)||(new TextDecoder().decode(bytes.slice(0,4))==='RIFF'&&new TextDecoder().decode(bytes.slice(8,12))==='WEBP');}
function readableHtml(html:string,documentId?:string):string {
  const clean=DOMPurify.sanitize(html,{FORBID_TAGS:['iframe','script','style','object','embed','form','input','button','select','textarea','base','meta','svg','audio','video','source','track','picture','link'],FORBID_ATTR:['style','srcset','background','poster','ping','action','formaction','srcdoc']});
  const document=new DOMParser().parseFromString(clean,'text/html');
  for(const image of Array.from(document.querySelectorAll('img'))){
    let allowed=false;
    try {const target=new URL(image.getAttribute('src')||'',window.location.origin),base='/api/documents/'+documentId;allowed=!!documentId&&target.origin===window.location.origin&&(target.pathname.startsWith(base+'/assets/')||target.pathname===base+'/media');}catch {}
    if(!allowed)image.remove();else {image.setAttribute('loading','lazy');image.setAttribute('decoding','async');}
  }
  return document.body.innerHTML;
}
function ReaderContent({html,markdown,text,documentId,markdownFile,mode}:{html:string;markdown:string;text:string;documentId?:string;markdownFile:boolean;mode:ReadingMode}) {
  const readingHtml=useMemo(()=>{
    // Text/CSS/XML imports also keep an export field named markdown. Preserve
    // their literal source unless this is a Markdown file or a distinct projection.
    const projection=markdown&&(markdownFile||markdown!==text)?markdown:'';
    const source=html||(projection?marked.parse(projection,{async:false,gfm:true,breaks:true}).trim():'');
    return source?readableHtml(source,documentId):'';
  },[html,markdown,text,documentId,markdownFile]);
  if(mode==='plain')return <article aria-label="Plain text" className="reading plain-reading">{text||'No plain text was found.'}</article>;
  if(readingHtml)return <article aria-label="Reading" className="reading" dangerouslySetInnerHTML={{__html:readingHtml}}/>;
  return <article aria-label="Reading" className="reading">{(text||markdown)?(text||markdown).split(/\n\s*\n/).map((paragraph,index)=><p className="text-paragraph" key={index}>{paragraph}</p>):<p>No readable text was found. You can open the original below.</p>}</article>;
}

export default function Workspace({userId}:{userId:string}) {
  const [queue,setQueue]=useState<QueueItem[]>([]),[selection,setSelection]=useState<Selection|null>(null),[documents,setDocuments]=useState<DocumentRow[]>([]);
  const [url,setUrl]=useState(''),[kind,setKind]=useState('file'),[paste,setPaste]=useState(''),[query,setQuery]=useState(''),[view,setView]=useState('text');
  const [readingMode,setReadingMode]=useState<ReadingMode>('reading'),[copied,setCopied]=useState<Extracted|null>(null),[folderSupported,setFolderSupported]=useState(false);
  const [settling,setSettling]=useState<string[]>([]),[storageBusy,setStorageBusy]=useState(false);
  const [deleteTarget,setDeleteTarget]=useState<DocumentRow|null>(null),[cleanupPending,setCleanupPending]=useState(false);
  const [error,setError]=useState(''),[recoveryWarning,setRecoveryWarning]=useState(''),[announcement,setAnnouncement]=useState(''),[queueOpen,setQueueOpen]=useState(true),[dragging,setDragging]=useState(false),[loading,setLoading]=useState(false),[restored,setRestored]=useState(false);
  const queueRef=useRef<QueueItem[]>([]),selectionRef=useRef<Selection|null>(null),running=useRef(false),mounted=useRef(true),generation=useRef(0),listGeneration=useRef(0),dirtyDraft=useRef(false),recoveryReady=useRef(false);
  const pendingDisposals=useRef(new Map<string,()=>Promise<void>>()),composerValue=useRef(paste);composerValue.current=paste;
  const registry=useRef(new ImportAttemptRegistry()),attempts=useRef(new Map<string,ImportAttempt>()),discoveries=useRef(new Set<AbortController>()),deletedDocuments=useRef(new Set<string>()),deletingDocuments=useRef(new Set<string>());
  const cleanedDocuments=useRef(new Set<string>());
  const pendingDeletion=useRef<PendingDeletion|null>(null);
  const storageOperation=useRef(false);
  const completions=useRef(new Map<string,{resolve:(value:unknown)=>void;reject:(reason:unknown)=>void}>());
  const checkpointTimer=useRef<ReturnType<typeof setTimeout>|null>(null);
  const pumpRef=useRef<()=>Promise<void>>(async()=>{}),checkpointRef=useRef<(strict?:boolean)=>Promise<void>>(async()=>{}),openSavedRef=useRef<(id:string,tab?:string,scroll?:number)=>Promise<void>>(async()=>{});
  const captureRef=useRef<(url:string,feed:boolean)=>Promise<unknown>>(async()=>{});
  const fileInput=useRef<HTMLInputElement>(null),folderInput=useRef<HTMLInputElement>(null),photoInput=useRef<HTMLInputElement>(null),resultHeading=useRef<HTMLHeadingElement>(null);
  const attachFolderInput=useCallback((element:HTMLInputElement|null)=>{folderInput.current=element;if(element){setFolderSupported('webkitdirectory'in element);element.setAttribute('webkitdirectory','');}},[]);
  const selectedItem=selection && 'queueId' in selection ? queue.find(item=>item.id===selection.queueId) : undefined;
  const selected=selectedItem?.record || (selection && 'record' in selection?selection.record:null);
  const result=selectedItem?.result || (selection && 'record' in selection?selection.result:null);
  const pending=queue.filter(item=>activePhases.has(item.phase)).length;
  const selectedHasWriters=!!selected&&queue.some(item=>item.record?.id===selected.id&&(activePhases.has(item.phase)||settling.includes(item.id)));
  const storageDocument=deleteTarget||selected;
  const storageDocumentHasWriters=!!storageDocument&&queue.some(item=>item.record?.id===storageDocument.id&&(activePhases.has(item.phase)||settling.includes(item.id)));
  const processing=[...queue].reverse().find(item=>activePhases.has(item.phase)&&item.phase!=='waiting');
  const newlyReady=[...queue].reverse().find(item=>item.result&&item.result.status!=='failed'&&item.result.metadata?.extractionAvailable!==false&&item.id!==selectedItem?.id&&(!item.record||item.record.id!==selected?.id));

  function choose(value:Selection|null) {selectionRef.current=value;if(mounted.current)setSelection(value);}
  function update(id:string,patch:Partial<QueueItem>) {
    queueRef.current=queueRef.current.map(item=>item.id===id?{...item,...patch}:item);
    if(mounted.current)setQueue(queueRef.current);
  }
  const refresh=useCallback(async(search='')=>{
    const request=++listGeneration.current;
    const data=await json<{documents:DocumentRow[]}>(await fetch('/api/documents?q='+encodeURIComponent(search)));
    if(request===listGeneration.current&&mounted.current)setDocuments(data.documents.filter(record=>!deletedDocuments.current.has(record.id)));
  },[]);
  function historySelection(documentId?:string,queueId?:string,tab='text',replace=false,mode:ReadingMode='reading') {
    const current={...(history.state||{}),tpe:{scroll:window.scrollY}};history.replaceState(current,'');
    const address=new URL(window.location.href);address.searchParams.delete('document');address.searchParams.delete('queue');
    if(documentId)address.searchParams.set('document',documentId);else if(queueId)address.searchParams.set('queue',queueId);
    address.searchParams.set('tab',tab);
    if(mode==='plain')address.searchParams.set('mode','plain');else address.searchParams.delete('mode');
    history[replace?'replaceState':'pushState']({...current,tpe:{scroll:replace?window.scrollY:0}},'',address);
  }
  function selectQueue(id:string,navigate=true) {
    const item=queueRef.current.find(value=>value.id===id);
    if(navigate&&selectionRef.current&&'queueId'in selectionRef.current&&selectionRef.current.queueId===id){
      generation.current++;setLoading(false);
      const address=new URL(window.location.href);if(address.searchParams.get('document')!==item?.record?.id&&address.searchParams.get('queue')!==id)historySelection(item?.record?.id,id,view,false,readingMode);
      return;
    }
    generation.current++;setLoading(false);choose({queueId:id});setView('text');setReadingMode('reading');
    if(navigate)historySelection(item?.record?.id,id);
    if(item?.record&&!item.result){void fetchRecord(item.record.id).then(response=>json<{record:DocumentRow;result:Extracted|null}>(response)).then(data=>{const current=queueRef.current.find(value=>value.id===id);if(current&&!current.result&&current.record?.id===data.record.id)update(id,{...(current.record===item.record?{record:data.record}:{}),...(data.result?{result:data.result}:{})});}).catch(reason=>{if(selectionRef.current&&'queueId'in selectionRef.current&&selectionRef.current.queueId===id)setError(messageOf(reason));});}
  }
  function focusReader(){requestAnimationFrame(()=>requestAnimationFrame(()=>{const reader=document.getElementById('reader');reader?.scrollIntoView?.({block:'start'});reader?.focus({preventScroll:true});}));}
  function readQueue(id:string){selectQueue(id);focusReader();}
  function fetchRecord(id:string):Promise<Response> {return fetch('/api/documents/'+encodeURIComponent(id));}
  async function openSaved(id:string,tab='text',scroll=0) {
    const request=++generation.current;setLoading(true);setError('');
    if(deletedDocuments.current.has(id)){choose(null);setLoading(false);setError('This saved document was deleted.');return;}
    const mode:ReadingMode=new URL(window.location.href).searchParams.get('mode')==='plain'?'plain':'reading';
    const existing=queueRef.current.find(item=>item.record?.id===id&&item.result);
    if(existing){choose({queueId:existing.id});setView(tab);setReadingMode(mode);setLoading(false);requestAnimationFrame(()=>window.scrollTo({top:scroll}));return;}
    try {const data=await json<{record:DocumentRow;result:Extracted|null}>(await fetchRecord(id));if(request!==generation.current||!mounted.current||deletedDocuments.current.has(id))return;choose(data);setView(tab);setReadingMode(mode);requestAnimationFrame(()=>requestAnimationFrame(()=>window.scrollTo({top:scroll})));}
    catch(reason){if(request===generation.current)setError(messageOf(reason));}
    finally{if(request===generation.current)setLoading(false);}
  }
  openSavedRef.current=openSaved;
  function openRecord(record:DocumentRow) {historySelection(record.id);void openSaved(record.id).then(()=>{const current=selectionRef.current;if(current&&('record'in current?current.record.id===record.id:queueRef.current.find(item=>item.id===current.queueId)?.record?.id===record.id))focusReader();});}

  function add(sources:{source:Source;name:string;parentId?:string}[],start=true):string[] {
    const items=sources.map(input=>({...input,id:crypto.randomUUID(),phase:'waiting' as const,progress:null,message:'Waiting to import.'}));
    queueRef.current=[...queueRef.current,...items];setQueue(queueRef.current);
    if(items.length)setAnnouncement(items.length===1?items[0].name+' added to imports.':items.length+' files added to imports.');
    if(items.length&&!selectionRef.current&&!loading)selectQueue(items[0].id);
    if(start)queueMicrotask(()=>void pumpRef.current());
    return items.map(item=>item.id);
  }
  function addFiles(files:Iterable<File>) {try{add(selectedImportFiles(files).map(({file,path})=>({source:{type:'file' as const,file},name:path})));}catch(reason){setError(messageOf(reason));}}
  function rereadOriginal() {
    if(!selected||pending||deletingDocuments.current.has(selected.id)||deletedDocuments.current.has(selected.id))return;
    const id=add([{source:{type:'stored',name:selected.original_name,url:selected.source_url||undefined},name:selected.title}],false)[0];
    update(id,{record:selected,...(result?{result}:{}),message:'Waiting to re-read the saved original.'});
    selectQueue(id);queueMicrotask(()=>void pumpRef.current());
  }
  function cancelItem(id:string) {
    const item=queueRef.current.find(value=>value.id===id);if(!item)return;
    if(item.phase==='waiting'&&!attempts.current.has(id)){update(id,cancelImportItem(item));completions.current.get(id)?.reject(new Error('Import cancelled.'));completions.current.delete(id);}
    else {registry.current.cancel(id);update(id,{message:item.phase==='saving'||item.phase==='uploading'?'Cancelling; waiting for storage to confirm its state.':'Cancelling…'});}
  }
  async function retry(id:string,saveOnly=false) {
    const item=queueRef.current.find(value=>value.id===id);
    if(!item||attempts.current.has(id))return;
    if(item.record&&(deletingDocuments.current.has(item.record.id)||deletedDocuments.current.has(item.record.id))){setError('This saved document is being deleted.');return;}
    if(item?.source.type==='file'&&item.source.member&&!item.record){
      try {const stored=await readWorkspace<Snapshot>(userId),copy=stored?.items.find(value=>value.id===id);if(copy?.source.type!=='file')throw new Error('Retry the original archive to recover this member.');update(id,{source:copy.source});}
      catch(reason){update(id,{error:messageOf(reason)});return;}
    }
    try{const current=queueRef.current.find(value=>value.id===id);if(!current)return;update(id,retryImportItem(current,!!current.record&&(saveOnly||!!current.savePending)));queueMicrotask(()=>void pumpRef.current());}catch(reason){setError(messageOf(reason));}
  }
  function beginStorageOperation(){if(!recoveryReady.current)throw new Error('Wait for local recovery to finish loading.');if(storageOperation.current)throw new Error('Another storage change is still finishing.');storageOperation.current=true;setStorageBusy(true);}
  function endStorageOperation(){storageOperation.current=false;if(mounted.current)setStorageBusy(false);}
  function recordHasWriters(id:string){return queueRef.current.some(item=>item.record?.id===id&&(activePhases.has(item.phase)||attempts.current.has(item.id)));}
  async function removeQueueItem(id:string){
    const index=queueRef.current.findIndex(item=>item.id===id),item=queueRef.current[index];if(!item)return;
    if(activePhases.has(item.phase)||attempts.current.has(id))throw new Error('Cancel this import and wait for it to settle before removing it.');
    beginStorageOperation();
    const before=selectionRef.current,address=window.location.href;
    let replacement:Selection|null=before;
    try{
      registry.current.remove(id);queueRef.current=queueRef.current.filter(value=>value.id!==id);setQueue(queueRef.current);
      if(before&&'queueId'in before&&before.queueId===id){generation.current++;setLoading(false);replacement=item.record?{record:item.record,result:item.result||null}:null;choose(replacement);historySelection(item.record?.id,undefined,view,true,readingMode);}
      await checkpointRef.current(true);
      const dispose=pendingDisposals.current.get(id);pendingDisposals.current.delete(id);if(dispose)void dispose().catch(reason=>setRecoveryWarning('The queue entry was removed, but its temporary file could not be cleared: '+messageOf(reason)));
      setAnnouncement(item.name+' removed from this queue. Saved documents are unchanged.');
    }catch(reason){
      if(!queueRef.current.some(value=>value.id===id)){queueRef.current.splice(Math.min(index,queueRef.current.length),0,item);setQueue([...queueRef.current]);}
      if(selectionRef.current===replacement){choose(before);history.replaceState(history.state,'',address);}
      setError('The queue entry could not be removed from local recovery. '+messageOf(reason));throw reason;
    }finally{endStorageOperation();}
  }
  async function clearSavedCopies(){
    beginStorageOperation();
    try{
      await checkpointRef.current(true);
      const cleared=await clearSavedWorkspaceCache<Snapshot>(userId);
      const ids=new Set((cleared.snapshot?.items||[]).filter(item=>item.phase==='saved'&&!item.savePending&&item.record).map(item=>item.record!.id));
      const current=selectionRef.current;
      const reading=current&&'queueId'in current?queueRef.current.find(item=>item.id===current.queueId):undefined;
      if(reading?.record&&reading.phase==='saved'&&!reading.savePending&&ids.has(reading.record.id))choose({record:reading.record,result:reading.result||null});
      queueRef.current=queueRef.current.map(item=>{
        if(item.phase!=='saved'||item.savePending||!item.record||!ids.has(item.record.id)||attempts.current.has(item.id))return item;
        const source:Source=item.source.type==='file'?{type:'stored',name:item.source.file.name,url:item.source.url,member:item.source.member}:item.source.type==='stored'?{type:'stored',name:item.source.name,url:item.source.url,member:item.source.member}:item.source;
        return {...item,source,result:undefined};
      });setQueue(queueRef.current);
      await checkpointRef.current(true);setAnnouncement('Saved copies cleared on this device. Unfinished imports and saved documents are kept.');
    }finally{endStorageOperation();}
  }
  function purgeDeletedRecord(record:DocumentRow,invalidateList=true){
    deletedDocuments.current.add(record.id);generation.current++;if(invalidateList)listGeneration.current++;setLoading(false);
    const removedIds=new Set(queueRef.current.filter(item=>item.record?.id===record.id).map(item=>item.id));
    for(const id of removedIds){registry.current.remove(id);completions.current.delete(id);}
    queueRef.current=queueRef.current.filter(item=>item.record?.id!==record.id);setQueue(queueRef.current);setDocuments(current=>current.filter(item=>item.id!==record.id));
    const current=selectionRef.current;if(current&&('queueId'in current?removedIds.has(current.queueId):current.record.id===record.id)){choose(null);historySelection(undefined,undefined,'text',true);}
  }
  async function deleteDocument(record:DocumentRow){
    if(recordHasWriters(record.id))throw new Error('Wait for imports using this document to finish before deleting it.');
    beginStorageOperation();deletingDocuments.current.add(record.id);setDeleteTarget(record);
    try{
      if(!cleanedDocuments.current.has(record.id))await deleteStoredDocument(record.id,{confirmDocumentId:record.id});
      cleanedDocuments.current.add(record.id);pendingDeletion.current=null;purgeDeletedRecord(record);
      await checkpointRef.current(true);setDeleteTarget(null);setCleanupPending(false);deletingDocuments.current.delete(record.id);setAnnouncement(record.title+' deleted from saved documents.');
    }catch(reason){
      let recoveryError='';
      if(reason instanceof StoredDocumentDeletionError&&reason.libraryRemoved){pendingDeletion.current={record,cleanupComplete:false};setCleanupPending(true);purgeDeletedRecord(record);try{await checkpointRef.current(true);}catch(failure){recoveryError=' Local recovery also needs a retry: '+messageOf(failure);}}
      else if(cleanedDocuments.current.has(record.id)){pendingDeletion.current={record,cleanupComplete:true};setCleanupPending(true);}
      else if(!deletedDocuments.current.has(record.id)){deletingDocuments.current.delete(record.id);setDeleteTarget(null);setCleanupPending(false);}
      const message=cleanedDocuments.current.has(record.id)?'The saved document was deleted. Retry to finish updating local recovery. ':deletedDocuments.current.has(record.id)?'Removed from your library. Retry Delete to finish storage cleanup. ':'Deletion needs a retry. ';
      setError(message+messageOf(reason)+recoveryError);throw new Error(message+messageOf(reason)+recoveryError);
    }
    finally{endStorageOperation();}
  }
  function currentAttempt(attempt:ImportAttempt,receipt=false){return mounted.current&&(receipt?registry.current.isLatest(attempt):registry.current.isCurrent(attempt))&&queueRef.current.some(item=>item.id===attempt.id);}
  async function persistItem(id:string,record:DocumentRow,extracted:Extracted,attempt:ImportAttempt) {
    const signal=attempt.signal;signal.throwIfAborted();if(!currentAttempt(attempt))throw new DOMException('Import interrupted.','AbortError');
    const value={...extracted,links:extracted.links.map(link=>({...link,doi:link.doi||doiFrom(link.url)}))};
    update(id,{result:value,savePending:true,phase:'saving',progress:0,message:'Saving the extracted result…'});
    await saveExtracted(record,value,{signal,onProgress:fraction=>{if(currentAttempt(attempt))update(id,{progress:100*fraction});}});
    if(!currentAttempt(attempt,true))return;
    const savedRecord={...record,title:value.title,status:value.status,engine:value.engine};
    update(id,{record:savedRecord,savePending:false,result:value,phase:'saved',retrySave:false,progress:100,error:undefined,message:signal.aborted?'Saved before cancellation finished.':'Original and result saved.'});
    // Every saved-document entry points to the current committed result, while
    // independent unsaved results retain their own retry payload.
    queueRef.current=queueRef.current.map(item=>item.record?.id===record.id&&!item.savePending?{...item,record:savedRecord,result:value}:item);setQueue(queueRef.current);
    void refresh(query).catch(reason=>setError('Saved, but the document list could not refresh: '+messageOf(reason)));
  }
  async function runItem(id:string,parentSignal?:AbortSignal):Promise<void> {
    let item=queueRef.current.find(value=>value.id===id);
    if(!item||attempts.current.has(id)||item.phase!=='waiting')return;
    const attempt=registry.current.start(id),signal=attempt.signal,completion=completions.current.get(id);attempts.current.set(id,attempt);setSettling([...attempts.current.keys()]);
    const publish=(patch:Partial<QueueItem>,receipt=false)=>{if(currentAttempt(attempt,receipt))update(id,patch);};
    const abort=()=>registry.current.cancel(id);parentSignal?.addEventListener('abort',abort,{once:true});if(parentSignal?.aborted)abort();
    const reusingOriginal=!!item.record;
    try {
      if(item.record&&!item.result){
        publish({phase:'fetching',message:'Opening the saved result…',progress:null});
        const saved=await json<{record:DocumentRow;result:Extracted|null}>(await fetch('/api/documents/'+item.record.id,{signal}));
        publish({record:saved.record,...(saved.result?{result:saved.result}:{})});item=queueRef.current.find(value=>value.id===id)!;
      }
      if(item.retrySave&&item.record&&item.result){await persistItem(id,item.record,item.result,attempt);publish({phase:'saved',message:'Result saved.',retrySave:false});return;}
      signal.throwIfAborted();let file:File,sourceUrl='',decoded:string|undefined;
      if(item.source.type==='url'){
        publish({phase:'fetching',message:'Fetching the public source…',progress:null});
        const captured=await captureSource(item.source.url,signal);file=captured.file;sourceUrl=captured.url;decoded=captured.decodedSource;
        publish({source:{type:'file',file,url:sourceUrl,decoded}});
      }else if(item.source.type==='stored'){
        if(!item.record)throw new Error('Reselect this source file to continue.');
        publish({phase:'fetching',message:'Opening the saved original…',progress:null});
        const response=await fetch('/api/documents/'+item.record.id+'/original',{signal});if(!response.ok)throw new Error('The saved original could not be reopened.');
        file=new File([await response.blob()],item.source.name,{type:response.headers.get('X-TPE-Original-Content-Type')||item.record.mime||''});sourceUrl=item.source.url||'';decoded=item.source.decoded;
      }else {file=item.source.file;sourceUrl=item.source.url||'';decoded=item.source.decoded;}
      signal.throwIfAborted();
      publish({phase:'extracting',progress:null,message:'Reading the source…'});
      const textDraft=reusingOriginal?null:await prepareTextImport(file,{sourceUrl,signal});
      if(textDraft){
        signal.throwIfAborted();publish({result:textDraft.result,savePending:true,message:'Text ready. Saving…'});
        // Yield a task for React to commit the preview before storage begins.
        await new Promise<void>(resolve=>setTimeout(resolve,0));signal.throwIfAborted();
      }
      let record=item.record;
      if(!record){publish({phase:'uploading',progress:0,message:'Saving the original…'});record=await uploadOriginal(file,{sourceUrl,signal,onProgress:fraction=>publish({progress:100*fraction})});publish({record},true);
        const current=selectionRef.current;if(currentAttempt(attempt,true)&&current&&'queueId'in current&&current.queueId===id){const address=new URL(window.location.href);if(address.searchParams.get('queue')===id||address.searchParams.get('document')===record.id)historySelection(record.id,id,address.searchParams.get('tab')||'text',true,address.searchParams.get('mode')==='plain'?'plain':'reading');}
      }
      signal.throwIfAborted();publish({phase:'extracting',progress:null,message:'Reading the saved source…'});
      if(!(textDraft&&record.kind==='text')&&decoded===undefined&&['html','feed','text','css','xml','json'].includes(record.kind))decoded=await decodeSource(file,file.type||record.mime||'',signal);
      let extracted:Extracted;
      if(textDraft&&record.kind==='text')extracted=textDraft.result;
      else if(record.kind==='archive'){
        const members:{path:string;status:string;documentId?:string;error?:string}[]=[];
        for await(const member of expandUploads([file],progress=>publish({message:progress.phase+' · '+progress.path,progress:progress.total?100*progress.completed/progress.total:null}),signal)){
          signal.throwIfAborted();
          if('error'in member){const child=add([{source:{type:'stored',name:member.path},name:member.path,parentId:id}],false)[0];update(child,{phase:'failed',error:member.error,message:'Archive member could not be read.'});members.push({path:member.path,status:'failed',error:member.error});continue;}
          const child=add([{source:{type:'file',file:member.file,member:true},name:member.path,parentId:id}],false)[0];
          try {await runItem(child,signal);const childItem=queueRef.current.find(value=>value.id===child)!;members.push({path:member.path,status:childItem.phase,documentId:childItem.record?.id,error:childItem.error});await checkpointRef.current();}
          finally {if(queueRef.current.find(value=>value.id===child)?.record)await member.dispose?.();else if(member.dispose)pendingDisposals.current.set(child,member.dispose);}
        }
        signal.throwIfAborted();
        extracted={title:file.name,text:members.map(member=>member.path+' — '+member.status).join('\n'),links:[],entries:members,metadata:{members},warnings:['Archive members are saved and processed separately. Review each member’s status.'],engine:'Archive expansion',status:'partial'};
      }else if(record.kind==='office'){
        const owner=record,assets=new Map<string,string>(),assetWarnings:string[]=[];
        const office=await extractOffice(file,signal,async asset=>{
          publish({message:'Saving embedded image: '+asset.file.name,progress:null});
          try {const stored=await uploadAssetFile(owner,asset.file,{signal});assets.set(asset.id,stored.url);}
          catch(reason){if(signal.aborted)throw reason;assetWarnings.push('Embedded image could not be saved: '+asset.file.name+' ('+messageOf(reason)+').');}
        });
        extracted=office.extracted;
        if(extracted.html){const document=new DOMParser().parseFromString(extracted.html,'text/html');for(const image of Array.from(document.querySelectorAll('img[data-image-id]'))){const source=assets.get(image.getAttribute('data-image-id')||'');if(source)image.setAttribute('src',source);else image.remove();}extracted={...extracted,html:document.body.innerHTML};}
        extracted={...extracted,warnings:[...extracted.warnings,...assetWarnings],status:assetWarnings.length?'partial':extracted.status,metadata:{...extracted.metadata,retainedImages:Array.from(assets,([imageId,url])=>({imageId,url}))}};
      }else if(record.kind==='pdf')extracted=await pdf(await file.arrayBuffer(),file.name,signal,(completed,total)=>publish({progress:total?100*completed/total:null,message:'Extracting page '+completed+' of '+total+'.'}));
      else if(record.kind==='image'&&await supportsOcr(file))extracted=await recognizeImage(file,(event:{status:string;progress:number})=>publish({message:event.status,progress:event.progress*100}),signal);
      else if(record.kind==='feed')extracted=parseFeed(decoded??await file.text(),sourceUrl||'https://saved.invalid/');
      else if(record.kind==='json')extracted=nativeRecord(decoded??await file.text(),file.name);
      else if(record.kind==='html')extracted=clipHtml(decoded??await file.text(),sourceUrl||'https://saved.invalid/',file.name);
      else if(record.kind==='text'||record.kind==='css'||record.kind==='xml'){
        const text=decoded??await file.text();extracted={title:file.name,text,markdown:text,links:textDois(text),warnings:[],metadata:{sourceUrl:sourceUrl||null,contentType:record.kind},engine:'Plain text decoder',status:'ready'};
      }else extracted={title:file.name,text:'',links:[],warnings:['The original is saved. Text extraction is not available for this file type.'],metadata:{extractionAvailable:false,contentType:record.kind},engine:'Original storage; no text extraction',status:'partial'};
      if(reusingOriginal&&extracted.status==='failed')throw new Error(extracted.warnings.join(' ')||'Re-reading did not produce a usable result.');
      signal.throwIfAborted();publish({result:extracted,savePending:true});if(extracted.html){publish({message:'Saving article images…',progress:null});extracted=await retainArticleImages(record,extracted,signal,(done,total)=>publish({message:'Saving image '+done+' of '+total+'.',progress:total?100*done/total:null}));}
      signal.throwIfAborted();await persistItem(id,record,extracted,attempt);
      if(!currentAttempt(attempt,true))return;
      publish({phase:'saved',message:'Original and result saved.',error:undefined});setAnnouncement(file.name+' saved.');
      completions.current.get(id)?.resolve({id:record.id,title:extracted.title,status:extracted.status,links:extracted.links.length});
    }catch(reason){
      item=queueRef.current.find(value=>value.id===id);
      if(!item||!currentAttempt(attempt,true))return;
      const cancelled=signal.aborted;const detail=cancelled?'Import cancelled.':messageOf(reason);
      if(!cancelled&&item.record&&!item.savePending&&!reusingOriginal){
        const failed:Extracted={title:item.name,text:'',links:[],warnings:[detail],engine:'Import stopped before an extraction result was available',status:'failed'};
        try {await persistItem(id,item.record,failed,attempt);}catch {publish({result:failed,savePending:true},true);}
      }
      item=queueRef.current.find(value=>value.id===id)!;
      if(cancelled){publish({...cancelImportItem(item),error:detail},true);}
      else publish({phase:'failed',error:detail,progress:null,message:item.savePending?'The extracted result is retained here. Retry or export it.':reusingOriginal?'The previously saved result is unchanged.':item.record?'The original remains saved.':'The source could not be imported.'},true);
      setAnnouncement(item.name+': '+detail);completion?.reject(new Error(detail));
    }finally {
      parentSignal?.removeEventListener('abort',abort);
      if(attempts.current.get(id)===attempt){
        if(queueRef.current.find(value=>value.id===id)?.record&&pendingDisposals.current.has(id)){const dispose=pendingDisposals.current.get(id)!;pendingDisposals.current.delete(id);await dispose().catch(()=>{});}
        registry.current.finish(attempt);attempts.current.delete(id);if(completions.current.get(id)===completion)completions.current.delete(id);
        if(mounted.current)setSettling([...attempts.current.keys()]);void checkpointRef.current();
      }
    }
  }
  pumpRef.current=async()=>{
    if(running.current)return;running.current=true;
    try {while(mounted.current){const next=queueRef.current.find(item=>item.phase==='waiting');if(!next)break;await runItem(next.id);}}
    finally {running.current=false;}
  };
  captureRef.current=(source,feed)=>{const id=add([{source:{type:'url',url:source,feed},name:source}])[0];return new Promise((resolve,reject)=>completions.current.set(id,{resolve,reject}));};

  function addText(text:string,html='') {
    const clean=text.trim();if(!clean&&!html)return;
    const lines=clean.split(/\r?\n/).filter(Boolean);if(lines.length&&lines.every(line=>/^https?:\/\//i.test(line)&&safeUrl(line))){add(lines.map(value=>({source:{type:'url' as const,url:value,feed:kind==='feed'},name:value})));return;}
    addFiles([new File([html||text],html?'Pasted page.html':'Pasted text.txt',{type:html?'text/html':'text/plain'})]);
  }
  function onPaste(event:ClipboardEvent) {
    const files=Array.from(event.clipboardData.files);if(files.length){event.preventDefault();addFiles(files);return;}
    if((event.target as HTMLElement).closest('input,textarea,[contenteditable=true]'))return;
    const text=event.clipboardData.getData('text/plain'),html=event.clipboardData.getData('text/html');if(text||html){event.preventDefault();addText(text,html);}
  }
  async function drop(event:DragEvent) {
    event.preventDefault();setDragging(false);
    if(event.dataTransfer.files.length||Array.from(event.dataTransfer.items).some(item=>item.kind==='file')){
      const controller=new AbortController();discoveries.current.add(controller);
      // Capture browser handles before the first asynchronous boundary.
      const files=filesFromDrop(event.dataTransfer,controller.signal);
      try {for await(const item of files){if(!mounted.current)return;if('file'in item)add([{source:{type:'file',file:item.file},name:item.path}]);else{const id=add([{source:{type:'stored',name:item.path},name:item.path}],false)[0];update(id,{phase:'failed',message:'Reselect this source to try again.',error:'Could not read '+item.path+': '+item.error});}}}
      catch(reason){if(!controller.signal.aborted)setError(messageOf(reason));}
      finally{discoveries.current.delete(controller);}
    }else addText(event.dataTransfer.getData('text/uri-list').split('\n').filter(line=>!line.startsWith('#')).join('\n')||event.dataTransfer.getData('text/plain'),event.dataTransfer.getData('text/html'));
  }

  function snapshot():Snapshot {
    return {version:1,items:queueRef.current.map(item=>({ ...item,source:item.record&&item.source.type==='file'?{type:'stored',name:item.source.file.name,url:item.source.url,decoded:item.source.decoded,member:item.source.member}:item.source,result:item.savePending?item.result:undefined})),draft:{url,kind,paste,query},selection:selectionRef.current?'queueId'in selectionRef.current?{queueId:selectionRef.current.queueId}:{documentId:selectionRef.current.record.id}:null,view,readingMode,scroll:window.scrollY,...(pendingDeletion.current?{pendingDeletion:pendingDeletion.current}:{})};
  }
  checkpointRef.current=async(strict=false)=>{
    if(!recoveryReady.current){if(strict)throw new Error('Wait for local recovery to finish loading.');return;}
    try{await writeWorkspace(userId,snapshot());if(mounted.current)setRecoveryWarning('');}
    catch(reason){if(mounted.current)setRecoveryWarning('This browser could not save a recovery copy: '+messageOf(reason)+'. Originals already saved remain in Saved documents.');if(strict)throw reason;}
  };
  useEffect(()=>{
    mounted.current=true;void refresh().catch(reason=>setError(messageOf(reason)));
    const restoreLocation=(fallback?:Snapshot)=>{
      const address=new URL(window.location.href),id=address.searchParams.get('document'),queueId=address.searchParams.get('queue');
      const tab=address.searchParams.get('tab')||fallback?.view||'text',scroll=history.state?.tpe?.scroll??fallback?.scroll??0;
      const mode:ReadingMode=(address.searchParams.get('mode')||fallback?.readingMode)==='plain'?'plain':'reading';
      if(id)void openSavedRef.current(id,tab,scroll);
      else if(queueId&&queueRef.current.some(item=>item.id===queueId)){selectQueue(queueId,false);setView(tab);setReadingMode(mode);requestAnimationFrame(()=>window.scrollTo({top:scroll}));}
      else if(fallback?.selection?.documentId){historySelection(fallback.selection.documentId,undefined,tab,true,mode);void openSavedRef.current(fallback.selection.documentId,tab,scroll);}
      else if(fallback?.selection?.queueId&&queueRef.current.some(item=>item.id===fallback.selection!.queueId)){const item=queueRef.current.find(item=>item.id===fallback.selection!.queueId)!;historySelection(item.record?.id,item.id,tab,true,mode);selectQueue(item.id,false);setView(tab);setReadingMode(mode);}
      else {generation.current++;setLoading(false);choose(null);setView(tab);setReadingMode(mode);}
    };
    void readWorkspace<Snapshot>(userId).then(saved=>{
      if(!mounted.current)return;
      if(saved?.version===1){if(saved.pendingDeletion){pendingDeletion.current=saved.pendingDeletion;const id=saved.pendingDeletion.record.id;deletedDocuments.current.add(id);deletingDocuments.current.add(id);if(saved.pendingDeletion.cleanupComplete)cleanedDocuments.current.add(id);setDeleteTarget(saved.pendingDeletion.record);setCleanupPending(true);purgeDeletedRecord(saved.pendingDeletion.record,false);}queueRef.current=restoreImportItems(saved.items.filter(item=>!item.record||!deletedDocuments.current.has(item.record.id)),queueRef.current.filter(item=>!item.record||!deletedDocuments.current.has(item.record.id)));setQueue(queueRef.current);if(!dirtyDraft.current){setUrl(saved.draft.url);setKind(saved.draft.kind);setPaste(saved.draft.paste);setQuery(saved.draft.query);if(saved.draft.query)void refresh(saved.draft.query).catch(reason=>setError(messageOf(reason)));}if(!selectionRef.current)restoreLocation(saved);}
      else if(!selectionRef.current)restoreLocation();
    }).catch(reason=>{setRecoveryWarning('Local recovery is unavailable: '+messageOf(reason));restoreLocation();}).finally(()=>{recoveryReady.current=true;if(mounted.current)setRestored(true);});
    const pop=()=>restoreLocation();const checkpoint=()=>{history.replaceState({...history.state,tpe:{scroll:window.scrollY}},'');void checkpointRef.current();};
    const visibility=()=>{if(document.visibilityState==='hidden')checkpoint();};
    let scrollTimer:ReturnType<typeof setTimeout>|undefined;
    const scroll=()=>{clearTimeout(scrollTimer);scrollTimer=setTimeout(()=>history.replaceState({...history.state,tpe:{scroll:window.scrollY}},''),150);};
    window.addEventListener('popstate',pop);window.addEventListener('pagehide',checkpoint);window.addEventListener('scroll',scroll,{passive:true});document.addEventListener('visibilitychange',visibility);
    return()=>{if(checkpointTimer.current)clearTimeout(checkpointTimer.current);checkpointTimer.current=null;checkpoint();mounted.current=false;registry.current.dispose();discoveries.current.forEach(controller=>controller.abort());discoveries.current.clear();window.removeEventListener('popstate',pop);window.removeEventListener('pagehide',checkpoint);window.removeEventListener('scroll',scroll);document.removeEventListener('visibilitychange',visibility);clearTimeout(scrollTimer);};
  // The owner-scoped workspace is restored once; callbacks read their current refs.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  },[userId,refresh]);
  useEffect(()=>{if(!restored||checkpointTimer.current)return;checkpointTimer.current=setTimeout(()=>{checkpointTimer.current=null;void checkpointRef.current();},250);},[queue,url,kind,paste,query,selection,view,readingMode,restored]);
  useEffect(()=>{
    const context=(document as Document&{modelContext?:{registerTool:(tool:unknown,options:unknown)=>unknown}}).modelContext;if(!context?.registerTool)return;
    const lifecycle=new AbortController();try{Promise.resolve(context.registerTool({name:'capture_source',title:'Capture a web page or feed',description:'Privately save a public source, extract it, and retain its import status.',inputSchema:{type:'object',properties:{url:{type:'string'},feed:{type:'boolean'}},required:['url'],additionalProperties:false},annotations:{readOnlyHint:false,untrustedContentHint:true},execute:async(input:unknown)=>{if(!input||typeof input!=='object'||!('url'in input)||typeof input.url!=='string'||!safeUrl(input.url))throw new Error('A public HTTP or HTTPS URL is required.');return captureRef.current(input.url,'feed'in input&&input.feed===true);}},{signal:lifecycle.signal})).catch(()=>{});}catch{}return()=>lifecycle.abort();
  },[]);
  function changeView(value:string){setView(value);const address=new URL(window.location.href);address.searchParams.set('tab',value);history.replaceState({...history.state,tpe:{scroll:window.scrollY}},'',address);}
  function changeReadingMode(value:ReadingMode){setReadingMode(value);const address=new URL(window.location.href);if(value==='plain')address.searchParams.set('mode','plain');else address.searchParams.delete('mode');history.replaceState({...history.state,tpe:{scroll:window.scrollY}},'',address);}
  const saveState=selectedItem?selectedItem.savePending?activePhases.has(selectedItem.phase)?'Ready to read · finishing save…':'Ready to read · not yet saved':selectedItem.phase==='saved'?'Saved privately':selectedItem.message:'Saved privately';
  async function copyMarkdown() {
    if(!result)return;
    try {if(!navigator.clipboard?.writeText)throw new Error('Clipboard access is unavailable.');await navigator.clipboard.writeText(result.markdown||result.text);if(mounted.current){setCopied(result);setAnnouncement(result.title+' Markdown copied.');}}
    catch {if(mounted.current){setError('Markdown could not be copied. Use Download Markdown to keep a copy.');setAnnouncement('Copy failed. Download Markdown is still available.');}}
  }
  async function detectClipboardUrl() {
    if(paste.trim()||!navigator.clipboard?.readText)return;
    try {const value=(await navigator.clipboard.readText()).trim();if(/^https?:\/\//i.test(value)&&safeUrl(value)&&!composerValue.current.trim()){dirtyDraft.current=true;setPaste(value);setAnnouncement('Link found on your clipboard. Send to import it.');}}catch { /* Clipboard access is optional; normal paste always works. */ }
  }
  function submitComposer() {if(paste.trim()){addText(paste);dirtyDraft.current=true;setPaste('');}}

  return <main onPaste={onPaste} onDragOver={event=>{event.preventDefault();setDragging(true);}} onDragLeave={event=>{if(!(event.relatedTarget instanceof Node)||!event.currentTarget.contains(event.relatedTarget))setDragging(false);}} onDrop={event=>void drop(event)} className={dragging?'drop-active':''}>
    <a className="skip-link" href="#reader">Skip to reader</a>
    <header className="app-header"><div className="brand"><FileText aria-hidden="true"/><h1>TPE</h1></div><span className="privacy"><LockKeyhole size={16} aria-hidden="true"/>Private</span></header>
    <p className="sr-only" role="status" aria-live="polite" aria-atomic="true">{announcement}</p>
    <div className="workspace-grid"><aside className="intake" aria-label="Add sources">
      <form className="composer" onSubmit={event=>{event.preventDefault();submitComposer();}}>
        <label htmlFor="source-paste" className="sr-only">Paste a link or text</label>
        <textarea id="source-paste" rows={2} value={paste} onFocus={()=>void detectClipboardUrl()} onChange={event=>{dirtyDraft.current=true;setPaste(event.target.value);}} onKeyDown={event=>{if(event.key==='Enter'&&(event.metaKey||event.ctrlKey)){event.preventDefault();submitComposer();}}} placeholder="Paste a link or text…"/>
        <div className="composer-actions"><details className="add-menu"><summary><Upload aria-hidden="true"/><span>Upload</span><ChevronDown aria-hidden="true"/></summary><div className="add-menu-options"><button type="button" onClick={event=>{event.currentTarget.closest('details')?.removeAttribute('open');fileInput.current?.click();}}><Upload aria-hidden="true"/>Add files</button><button type="button" disabled={!folderSupported} onClick={event=>{event.currentTarget.closest('details')?.removeAttribute('open');folderInput.current?.click();}}><FolderOpen aria-hidden="true"/>Add folder</button><button type="button" onClick={event=>{event.currentTarget.closest('details')?.removeAttribute('open');photoInput.current?.click();}}><ImagePlus aria-hidden="true"/>Add photos</button><p className="help">{folderSupported?'Choose multiple files or one folder at a time. Each selection joins the same queue.':'Folder picking is unavailable in this browser. Add files, or drop folders where supported.'}</p></div></details><span className="composer-hint">Or drop files here</span><Button type="submit" disabled={!paste.trim()} aria-label="Import pasted source" className="send-button"><ArrowUp aria-hidden="true"/></Button></div>
        <input ref={fileInput} hidden type="file" multiple aria-label="Choose source files" onChange={event=>{if(event.target.files)addFiles(event.target.files);event.target.value='';}}/>
        <input ref={attachFolderInput} hidden type="file" multiple aria-label="Choose a folder" onChange={event=>{if(event.target.files)addFiles(event.target.files);event.target.value='';}}/>
        <input ref={photoInput} hidden type="file" accept="image/*" multiple aria-label="Choose photos" onChange={event=>{if(event.target.files)addFiles(event.target.files);event.target.value='';}}/>
      </form>
      <p className="intake-hint help">Files and folders share one import queue. Drop both together where your browser supports it.</p>
      {!!pending&&<div className="processing-status" role="status" aria-live="polite" aria-atomic="true"><LoaderCircle className="processing-spinner" aria-hidden="true"/><div><strong>{processing?phaseLabel(processing.phase):'Waiting to import'}</strong><span>{processing?.name||pending+' queued'}{pending>1?' · '+pending+' imports remaining':''}</span>{processing&&!queueOpen&&<StageProgress item={processing}/>}</div></div>}
      {newlyReady&&<div className="ready-action"><Button variant="outline" onClick={()=>readQueue(newlyReady.id)}>Open {newlyReady.result?.title||newlyReady.name}</Button></div>}
      {error&&<div className="notice error" role="alert"><AlertCircle/><p>{error}</p><button className="icon-button" onClick={()=>setError('')} aria-label="Dismiss message"><X/></button></div>}
      {recoveryWarning&&<div className="notice" role="status"><AlertCircle/><p>{recoveryWarning}</p><Button variant="outline" disabled={!restored||storageBusy} onClick={()=>void checkpointRef.current(true).catch(()=>{})}>Retry recovery save</Button></div>}
      {!!queue.length&&<section className="queue-panel" aria-label="Imports"><button className="section-toggle" aria-expanded={queueOpen} onClick={()=>setQueueOpen(!queueOpen)}><span>{pending?'Imports · '+pending+' in progress':'Recent imports'}</span><ChevronDown aria-hidden="true"/></button><ImportBatchProgress items={queue}/>{queueOpen&&<ol className="queue-list">{queue.map(item=><li key={item.id} className={'queue-item '+(selectedItem?.id===item.id?'selected':'')}><div className="queue-row"><button className="queue-open" onClick={()=>readQueue(item.id)} aria-current={selectedItem?.id===item.id?true:undefined}><strong>{item.name}</strong><span className={'queue-phase phase-'+item.phase}>{phaseLabel(item.phase)}</span></button></div>{activePhases.has(item.phase)&&item.phase!=='waiting'&&<StageProgress item={item}/>}<div className="queue-actions"><ImportItemControls item={{...item,savePending:!!item.record&&item.savePending}} onCancel={()=>cancelItem(item.id)} onRetry={()=>void retry(item.id,!!item.savePending)} onRemove={()=>removeQueueItem(item.id)} canRetry={!settling.includes(item.id)&&(item.source.type!=='stored'||!!item.record)} removeDisabled={storageBusy||!restored||settling.includes(item.id)} hasLocalOnlyData={!!item.savePending||(item.source.type==='file'&&!item.record)}/></div>{item.error&&<p className="queue-error">{item.error}</p>}</li>)}</ol>}</section>}

      <details className="library" open={deleteTarget?true:undefined}><summary>Saved documents</summary><div className="library-actions">{deleteTarget&&cleanupPending&&<p className="help">Cleanup pending for “{deleteTarget.title}”. Use Delete saved document to retry; it has already been removed from your library.</p>}<ClearCachedFilesButton onClear={clearSavedCopies} disabled={!restored||storageBusy}/>{storageDocument&&<DeleteStoredDocumentButton documentName={storageDocument.title} onDelete={()=>deleteDocument(storageDocument)} disabled={!restored||storageBusy||storageDocumentHasWriters}/>}</div><form className="search-form" onSubmit={event=>{event.preventDefault();void refresh(query).catch(reason=>setError(messageOf(reason)));}}><label className="sr-only" htmlFor="search">Search saved documents</label><input id="search" value={query} onChange={event=>{dirtyDraft.current=true;setQuery(event.target.value);}} placeholder="Search"/><Button type="submit" variant="outline" aria-label="Search"><Search/></Button></form><div className="document-list">{documents.length?documents.map(record=><button key={record.id} aria-current={selected?.id===record.id?true:undefined} className={'document-item '+(selected?.id===record.id?'selected':'')} onClick={()=>openRecord(record)}><strong>{record.title}</strong><span className="help">{record.status==='uploaded'?'Original saved':record.status==='failed'?'Needs attention':'Saved'}</span></button>):<p className="help">Your saved sources appear here.</p>}</div></details>
    </aside><section id="reader" className="result-pane" aria-label="Document reader" aria-busy={!restored||loading} tabIndex={-1}>
      {loading&&selection&&<p role="status">Opening document…</p>}
      {!selection?!restored||loading?<div className="empty-state" role="status"><h2>{loading?'Opening document…':'Restoring your workspace…'}</h2></div>:<div className="empty-state"><h2>Bring your reading here.</h2><p>Paste a link or use Upload to add files. Your reading appears here as soon as it is ready.</p></div>:<>
        <div className="reader-heading"><div><p className="help" role="status" aria-live="polite">{saveState}</p><h2 ref={resultHeading} tabIndex={-1}>{result?.title||selected?.title||selectedItem?.name||'Document'}</h2></div></div>
        {selectedItem?.savePending&&!activePhases.has(selectedItem.phase)&&<div className="notice"><p>Your result is ready here but has not been saved. You can copy or download it now.</p><Button disabled={settling.includes(selectedItem.id)} onClick={()=>retry(selectedItem.id,!!selectedItem.record)}>{selectedItem.record?'Retry save':'Retry import'}</Button></div>}
        {selected?.kind==='image'&&<img className="original-image" src={'/api/documents/'+selected.id+'/media'} alt={selected.title} loading="lazy"/>}
        {selected&&['media','audio','video'].includes(selected.kind)&&(/\.(mp3|wav|m4a|aac|oga|flac|opus)$/i.test(selected.original_name)||selected.kind==='audio'?<audio className="original-media" controls preload="metadata" src={'/api/documents/'+selected.id+'/media'}/>:<video className="original-media" controls playsInline preload="metadata" src={'/api/documents/'+selected.id+'/media'}/>)}
        {result?<>
          {result.status!=='failed'&&result.metadata?.extractionAvailable!==false&&<div className="reader-toolbar"><div className="reading-modes" role="group" aria-label="Reading mode"><button aria-pressed={readingMode==='reading'} onClick={()=>changeReadingMode('reading')}>Reading</button><button aria-pressed={readingMode==='plain'} onClick={()=>changeReadingMode('plain')}>Plain text</button></div><div className="markdown-actions"><Button variant="outline" onClick={()=>void copyMarkdown()}>{copied===result?<Check aria-hidden="true"/>:<Copy aria-hidden="true"/>}Copy Markdown</Button><Button variant="outline" onClick={()=>download('extraction.md',result.markdown||result.text,'text/markdown')}><Download aria-hidden="true"/>Download Markdown</Button></div></div>}
          {result.status==='failed'?<div className="notice error"><AlertCircle/><p>{result.warnings.join(' ')}</p></div>:result.metadata?.extractionAvailable===false?<p>The original is saved. Text extraction is not available for this file type.</p>:<ReaderContent html={result.html||''} markdown={result.markdown||''} text={result.text} documentId={selected?.id} markdownFile={/\.(md|markdown)$/i.test(selected?.original_name||selectedItem?.name||'')} mode={readingMode}/>}
          <div className="reader-secondary"><details><summary>Original and other downloads</summary><div className="export-actions">{selected&&<Button asChild variant="outline"><a href={'/api/documents/'+selected.id+'/original'}><Download aria-hidden="true"/>Original</a></Button>}<Button variant="outline" onClick={()=>download('extraction.json',JSON.stringify({source:selected,...result},null,2))}>JSON</Button></div></details>
            {!!result.links.length&&<details open={view==='links'} onToggle={event=>{if(event.currentTarget.open&&view!=='links')changeView('links');else if(!event.currentTarget.open&&view==='links')changeView('text');}}><summary>Source links</summary><div className="link-list">{result.links.map((link,index)=><div key={index} className="link-card">{link.label&&<p>{link.label}</p>}{safeUrl(link.url)?<a href={link.url} target="_blank" rel="noreferrer noopener">{link.url}</a>:<code>{link.url}</code>}</div>)}</div></details>}
            <details open={view==='evidence'} onToggle={event=>{if(event.currentTarget.open&&view!=='evidence')changeView('evidence');else if(!event.currentTarget.open&&view==='evidence')changeView('text');}}><summary>Details and review notes</summary><dl className="evidence"><dt>Engine</dt><dd>{result.engine}</dd>{selected&&<><dt>Original SHA-256</dt><dd className="hash">{selected.sha256||'Available after storage verification'}</dd><dt>Saved</dt><dd>{new Date(selected.created_at).toLocaleString()}</dd>{selected.source_url&&<><dt>Source</dt><dd>{selected.source_url}</dd></>}</>}</dl>{result.warnings.length>0&&<ul className="review-notes">{result.warnings.map((warning,index)=><li key={index}>{warning}</li>)}</ul>}<details><summary>Structured data</summary><pre className="code-panel">{JSON.stringify({metadata:result.metadata,tables:result.tables},null,2)}</pre></details>{selected&&<div className="document-actions"><Button variant="outline" disabled={!!pending||storageBusy||selectedHasWriters} onClick={rereadOriginal}>Re-read original</Button></div>}</details>
          </div>
        </>:<div className="notice"><p>{selectedItem?.message||'The original is saved. No extraction result is available yet.'}</p>{selected&&<><Button asChild variant="outline"><a href={'/api/documents/'+selected.id+'/original'}>Open original</a></Button><Button variant="outline" disabled={!!pending||storageBusy||selectedHasWriters} onClick={rereadOriginal}>Re-read original</Button></>}</div>}
      </>}
    </section></div>
  </main>;
}
