export type LinkEvidence = { url: string; label?: string; page?: number; kind?: string; doi?: string; rect?: unknown };
export type Extracted = {
  title: string; text: string; markdown?: string; html?: string;
  links: LinkEvidence[]; warnings: string[]; engine: string; status: 'ready'|'partial'|'failed';
  metadata?: Record<string, unknown>; pages?: unknown[]; tables?: unknown[]; entries?: unknown[];
};
export type DocumentRow = { mime?:string; id: string; title: string; kind: string; source_url: string | null; original_name: string; status: string; engine: string; created_at: string; sha256: string; bytes: number };
