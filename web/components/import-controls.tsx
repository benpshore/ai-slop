'use client';

import { useEffect, useId, useRef, useState, useSyncExternalStore } from 'react';
import type { ReactNode } from 'react';
import { Button } from '@/components/ui/button';
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle, AlertDialogTrigger } from '@/components/ui/alert-dialog';
import { filesFromDrop, importPhaseLabel, isActiveImport, selectedImportFiles, summarizeImports } from '@/lib/import-queue';
import type { ImportProgressItem, SelectedImportFile } from '@/lib/import-queue';

const subscribeCapabilities = () => () => {};
const browserFolderSupport = () => 'webkitdirectory' in document.createElement('input');
const serverFolderSupport = () => true;

/** Separate picker selections always append to the same queue. */
export function ImportPicker({ onFiles, disabled = false }: {
  onFiles: (files: SelectedImportFile[]) => void;
  disabled?: boolean;
}) {
  const fileInput = useRef<HTMLInputElement>(null), folderInput = useRef<HTMLInputElement>(null);
  const foldersSupported = useSyncExternalStore(subscribeCapabilities, browserFolderSupport, serverFolderSupport);
  const [error, setError] = useState('');
  const helpId = useId();
  function select(input: HTMLInputElement) {
    try { if (input.files?.length) { onFiles(selectedImportFiles(Array.from(input.files))); setError(''); } }
    catch (reason) { setError(reason instanceof Error ? reason.message : String(reason)); }
    finally { input.value = ''; } // Selecting the same file again is an intentional new import.
  }
  return <div className="import-picker">
    <div className="flex flex-wrap gap-2">
      <Button type="button" variant="outline" disabled={disabled} aria-describedby={helpId} onClick={() => fileInput.current?.click()}>Add files</Button>
      <Button type="button" variant="outline" disabled={disabled || !foldersSupported} aria-describedby={helpId} onClick={() => folderInput.current?.click()}>Add folder</Button>
    </div>
    <input ref={fileInput} className="sr-only" type="file" multiple disabled={disabled} aria-label="Choose source files" onChange={event => select(event.currentTarget)} />
    <input ref={element => { folderInput.current = element; element?.setAttribute('webkitdirectory', ''); }} className="sr-only" type="file" multiple disabled={disabled || !foldersSupported} aria-label="Choose a source folder" onChange={event => select(event.currentTarget)} />
    <p id={helpId} className="help">{foldersSupported ? 'Add files and folders in separate selections; each selection joins this queue. Drop several folders together where your browser supports it.' : 'This browser does not offer a folder picker. Add files in several selections; each selection joins this queue.'}</p>
    {error && <p role="alert" className="queue-error">{error}</p>}
  </div>;
}

/** Optional wrapper. Owners with an existing drop handler can use filesFromDrop directly. */
export function ImportDropZone({ onFile, onError, disabled = false, children }: {
  onFile: (file: SelectedImportFile) => void;
  onError: (path: string, error: string) => void;
  disabled?: boolean;
  children: ReactNode;
}) {
  const [dragging, setDragging] = useState(false);
  const scans = useRef(new Set<AbortController>());
  useEffect(() => () => { for (const scan of scans.current) scan.abort(); scans.current.clear(); }, []);
  return <div className={dragging ? 'drop-active' : undefined}
    onDragOver={event => {
      if (disabled || !Array.from(event.dataTransfer.types).includes('Files')) return;
      event.preventDefault(); event.stopPropagation(); setDragging(true);
    }}
    onDragLeave={event => { if (!(event.relatedTarget instanceof Node) || !event.currentTarget.contains(event.relatedTarget)) setDragging(false); }}
    onDrop={event => {
      setDragging(false);
      if (disabled || !Array.from(event.dataTransfer.types).includes('Files')) return;
      event.preventDefault(); event.stopPropagation();
      const controller = new AbortController(); scans.current.add(controller);
      const files = filesFromDrop(event.dataTransfer, controller.signal);
      void (async () => {
        try { for await (const item of files) { if ('file' in item) onFile(item); else onError(item.path, item.error); } }
        catch (reason) { if (!controller.signal.aborted) onError('Dropped selection', reason instanceof Error ? reason.message : String(reason)); }
        finally { scans.current.delete(controller); }
      })();
    }}>
    {children}
  </div>;
}

export function ImportBatchProgress({ items }: { items: readonly Pick<ImportProgressItem, 'phase'>[] }) {
  const count = summarizeImports(items);
  if (!count.total) return null;
  const parts = [`${count.saved} of ${count.total} saved`];
  if (count.active) parts.push(`${count.active} in progress${count.waiting ? ` (${count.waiting} waiting)` : ''}`);
  if (count.failed) parts.push(`${count.failed} need attention`);
  if (count.cancelled) parts.push(`${count.cancelled} cancelled`);
  if (count.interrupted) parts.push(`${count.interrupted} interrupted`);
  return <p className="help" role="status" aria-live="polite" aria-atomic="true">{parts.join(' · ')}</p>;
}

export function ImportItemProgress({ item }: { item: ImportProgressItem }) {
  const active = isActiveImport(item.phase), label = importPhaseLabel(item.phase);
  const value = item.progress !== null && Number.isFinite(item.progress) && item.progress >= 0 && item.progress <= 100 ? item.progress : undefined;
  const descriptionId = useId();
  return <div>
    <span className={'queue-phase phase-' + item.phase}>{label}</span>
    {active && item.phase !== 'waiting' && <progress max={100} value={value} aria-label={`${label} progress for ${item.name}`} aria-describedby={descriptionId} />}
    <p id={descriptionId} className="help">{item.message}{active && value !== undefined ? ` (${Math.round(value)}% of this step)` : ''}</p>
    {item.error && <p className="queue-error">{item.error}</p>}
  </div>;
}

function ConfirmActionButton({ label, title, description, confirmLabel, pendingLabel, onConfirm, disabled, destructive = false }: {
  label: string;
  title: string;
  description: string;
  confirmLabel: string;
  pendingLabel: string;
  onConfirm: () => Promise<unknown>;
  disabled?: boolean;
  destructive?: boolean;
}) {
  const [open, setOpen] = useState(false), [busy, setBusy] = useState(false), [error, setError] = useState('');
  const inFlight = useRef(false);
  return <AlertDialog open={open} onOpenChange={value => { if (!inFlight.current) { setOpen(value); setError(''); } }}>
    <AlertDialogTrigger asChild><Button type="button" size="sm" variant="outline" disabled={disabled}>{label}</Button></AlertDialogTrigger>
    <AlertDialogContent aria-busy={busy}>
      <AlertDialogHeader><AlertDialogTitle>{title}</AlertDialogTitle><AlertDialogDescription>{description}</AlertDialogDescription></AlertDialogHeader>
      {error && <p role="alert" className="queue-error">{error}</p>}
      <AlertDialogFooter>
        <AlertDialogCancel disabled={busy}>Keep</AlertDialogCancel>
        <AlertDialogAction disabled={busy || disabled} variant={destructive ? 'destructive' : 'default'} onClick={event => {
          event.preventDefault();
          if (inFlight.current || disabled) return;
          inFlight.current = true; setBusy(true); setError('');
          void Promise.resolve().then(onConfirm).then(() => setOpen(false), reason => setError(reason instanceof Error ? reason.message : String(reason)))
            .finally(() => { inFlight.current = false; setBusy(false); });
        }}>{busy ? pendingLabel : confirmLabel}</AlertDialogAction>
      </AlertDialogFooter>
    </AlertDialogContent>
  </AlertDialog>;
}

/** Removal is local queue removal. The callback must never call document DELETE. */
export function ImportItemControls({ item, onCancel, onRetry, onRemove, canRetry = true, removeDisabled = false, hasLocalOnlyData = false }: {
  item: ImportProgressItem;
  onCancel: () => void;
  onRetry: () => void;
  onRemove: () => Promise<unknown>;
  canRetry?: boolean;
  removeDisabled?: boolean;
  /** True for uncommitted File/Blob sources or other recovery data the caller would discard. */
  hasLocalOnlyData?: boolean;
}) {
  const active = isActiveImport(item.phase);
  const [error, setError] = useState('');
  const [removing, setRemoving] = useState(false), removal = useRef(false);
  return <div className="flex flex-wrap gap-2" role="group" aria-label={'Import actions for ' + item.name}>
    {active && <Button type="button" variant="outline" size="sm" onClick={onCancel} aria-label={'Cancel import of ' + item.name}>Cancel</Button>}
    {canRetry && ['failed', 'cancelled', 'interrupted'].includes(item.phase) && <Button type="button" variant="outline" size="sm" onClick={onRetry}>{item.savePending ? 'Save again' : 'Retry'}</Button>}
    {hasLocalOnlyData || item.savePending
      ? <ConfirmActionButton label="Remove from queue" title={'Remove “' + item.name + '” from this queue?'} description="This discards its unfinished recovery copy on this device. Any saved document and original remain in Saved documents." confirmLabel="Remove from queue" pendingLabel="Removing…" disabled={active || removeDisabled} onConfirm={onRemove} />
      : <Button type="button" variant="outline" size="sm" disabled={active || removeDisabled || removing} aria-label={'Remove ' + item.name + ' from queue'} onClick={() => {
        if (removal.current) return;
        removal.current = true; setRemoving(true); setError('');
        void Promise.resolve().then(onRemove).catch(reason => setError(reason instanceof Error ? reason.message : String(reason)))
          .finally(() => { removal.current = false; setRemoving(false); });
      }}>{removing ? 'Removing…' : 'Remove from queue'}</Button>}
    {error && <p role="alert" className="queue-error">{error}</p>}
  </div>;
}

/** Disable until all imports writing to this document have fully settled. */
export function DeleteStoredDocumentButton({ documentName, onDelete, disabled = false }: {
  documentName: string;
  onDelete: () => Promise<unknown>;
  disabled?: boolean;
}) {
  return <ConfirmActionButton label="Delete saved document" title={'Delete “' + documentName + '”?'} description="This deletes the saved original, extracted result and attachments from your saved documents. This cannot be undone." confirmLabel="Delete saved document" pendingLabel="Deleting…" onConfirm={onDelete} disabled={disabled} destructive />;
}

export function ClearCachedFilesButton({ onClear, disabled = false }: { onClear: () => Promise<unknown>; disabled?: boolean }) {
  return <ConfirmActionButton label="Clear saved copies" title="Clear saved copies on this device?" description="Clears local cached files and text for completed imports. Saved documents and originals stay in your library. Unfinished imports and unsaved results are kept for recovery." confirmLabel="Clear saved copies" pendingLabel="Clearing…" onConfirm={onClear} disabled={disabled} />;
}
