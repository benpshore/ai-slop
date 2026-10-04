// IndexedDB structured cloning retains File/Blob bytes across real navigation.
// Browser storage quotas are surfaced to the caller, never treated as success.
const database = 'tpe-private-workspace';
function openDatabase(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(database, 1);
    request.onupgradeneeded = () => request.result.createObjectStore('workspaces');
    request.onsuccess = () => resolve(request.result);
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
const writes = new Map<string, Promise<void>>();
export async function writeWorkspace<T>(userId: string, value: T): Promise<void> {
  // Snapshot now, not when a previous disk transaction eventually finishes.
  const snapshot = structuredClone(value);
  const previous = writes.get(userId) ?? Promise.resolve();
  const pending = previous.catch(() => {}).then(async () => {
    const db = await openDatabase();
    try {
      await new Promise<void>((resolve, reject) => {
        const transaction = db.transaction('workspaces', 'readwrite');
        transaction.objectStore('workspaces').put(snapshot, userId);
        transaction.oncomplete = () => resolve();
        transaction.onerror = () => reject(transaction.error);
        transaction.onabort = () => reject(transaction.error ?? new Error('Recovery save interrupted.'));
      });
    } finally { db.close(); }
  });
  writes.set(userId, pending);
  try { await pending; } finally { if (writes.get(userId) === pending) writes.delete(userId); }
}
