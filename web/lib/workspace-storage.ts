// IndexedDB structured cloning retains File/Blob bytes across real navigation.
// Local recovery is separate from the owner-protected R2/D1 document library.
const database = 'tpe-private-workspace';
type CachedItem = {phase:string;record?:{id:string};savePending?:boolean;result?:unknown;source:{type:string;name?:string;file?:File;decoded?:string;[key:string]:unknown};[key:string]:unknown};
type CachedWorkspace = {items:CachedItem[];[key:string]:unknown};
function openDatabase(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(database, 2);
    request.onupgradeneeded = () => {
      for (const name of ['workspaces','cachePolicies']) if (!request.result.objectStoreNames.contains(name)) request.result.createObjectStore(name);
    };
    request.onsuccess = () => {
      const db=request.result;
      db.onversionchange=()=>db.close();
      resolve(db);
    };
    request.onerror = () => reject(request.error);
    request.onblocked = () => reject(new Error('Close another TPE tab to enable import recovery.'));
  });
}
export async function readWorkspace<T>(userId: string): Promise<T | null> {
  const db = await openDatabase();
  try {
    return await new Promise<T | null>((resolve, reject) => {
      const transaction = db.transaction('workspaces', 'readonly');
      const request = transaction.objectStore('workspaces').get(userId);
      transaction.oncomplete = () => resolve((request.result as T | undefined) ?? null);
      transaction.onerror = () => reject(transaction.error);
      transaction.onabort = () => reject(transaction.error ?? new Error('Recovery read interrupted.'));
    });
  } finally { db.close(); }
}
function isWorkspace(value:unknown):value is CachedWorkspace {
  return !!value && typeof value==='object' && 'items' in value && Array.isArray(value.items);
}
function trimSavedCopies<T>(value:T, ids:Set<string>):T {
  if(!isWorkspace(value))return value;
  return {...value,items:value.items.map(item=>{
    if(item.phase!=='saved'||item.savePending||!item.record||!ids.has(item.record.id))return item;
    const {file,decoded,...source}=item.source;
    void decoded;
    return {...item,result:undefined,source:{...source,...(file?{type:'stored',name:file.name}:{})}};
  })};
}
const writes = new Map<string, Promise<unknown>>();
function serialize<T>(userId:string, action:()=>Promise<T>):Promise<T> {
  const pending=(writes.get(userId)??Promise.resolve()).catch(()=>{}).then(action);
  writes.set(userId,pending);
  void pending.finally(()=>{if(writes.get(userId)===pending)writes.delete(userId);}).catch(()=>{});
  return pending;
}
export async function writeWorkspace<T>(userId: string, value: T): Promise<void> {
  const snapshot = structuredClone(value);
  await serialize(userId,async()=>{
    const db=await openDatabase();
    try { await new Promise<void>((resolve,reject)=>{
      // Reading policy and writing in one transaction prevents another tab
      // from reintroducing saved bytes already cleared by this owner.
      const tx=db.transaction(['workspaces','cachePolicies'],'readwrite');
      const policy=tx.objectStore('cachePolicies').get(userId);
      policy.onsuccess=()=>tx.objectStore('workspaces').put(trimSavedCopies(snapshot,new Set(policy.result??[])),userId);
      tx.oncomplete=()=>resolve();tx.onerror=()=>reject(tx.error);
      tx.onabort=()=>reject(tx.error??new Error('Recovery save interrupted.'));
    }); } finally {db.close();}
  });
}
/** Clear only redundant confirmed-saved copies for this owner. Never erase
 * pending sources/results, drafts, another owner, OPFS, or server documents.
 * Apply returned snapshot to UI state before its next checkpoint. */
export async function clearSavedWorkspaceCache<T>(userId:string):Promise<{snapshot:T|null;clearedItems:number}> {
  return serialize(userId,async()=>{
    const db=await openDatabase();
    try {return await new Promise<{snapshot:T|null;clearedItems:number}>((resolve,reject)=>{
      const tx=db.transaction(['workspaces','cachePolicies'],'readwrite');
      const values=tx.objectStore('workspaces'),policies=tx.objectStore('cachePolicies');
      const read=values.get(userId),policy=policies.get(userId);
      let snapshot:T|null=null,clearedItems=0;
      policy.onsuccess=()=>{
        const value=read.result;
        if(!isWorkspace(value)){snapshot=value??null;return;}
        const ids=new Set<string>(policy.result??[]);
        for(const item of value.items)if(item.phase==='saved'&&!item.savePending&&item.record){
          ids.add(item.record.id);
          if(item.source.file||item.source.decoded||item.result)clearedItems++;
        }
        snapshot=trimSavedCopies(value,ids) as T;
        values.put(snapshot,userId);policies.put([...ids],userId);
      };
      tx.oncomplete=()=>resolve({snapshot,clearedItems});tx.onerror=()=>reject(tx.error);
      tx.onabort=()=>reject(tx.error??new Error('Cache cleanup interrupted.'));
    });}finally{db.close();}
  });
}
