/**
 * Demarch platform tools for non-Claude runtimes (Gemini, Codex).
 * Wraps the IPC query bridge into tool functions matching the ipc-tools.ts pattern.
 */

import { queryKernel } from './ipc-demarch.js';
import type { IpcContext } from './ipc-tools.js';

export function demarchRunStatus(_ctx: IpcContext, runId?: string): Promise<string> {
  return queryKernel('run_status', runId ? { runId } : {});
}

export function demarchSprintPhase(_ctx: IpcContext): Promise<string> {
  return queryKernel('sprint_phase', {});
}

export function demarchSearchBeads(
  _ctx: IpcContext,
  id?: string,
  query?: string,
  status?: string,
): Promise<string> {
  const params: Record<string, unknown> = {};
  if (id) params.id = id;
  if (query) params.query = query;
  if (status) params.status = status;
  return queryKernel('search_beads', params);
}

export function demarchSpecLookup(_ctx: IpcContext, artifactId?: string): Promise<string> {
  return queryKernel('spec_lookup', artifactId ? { artifactId } : {});
}

export function demarchReviewSummary(_ctx: IpcContext): Promise<string> {
  return queryKernel('review_summary', {});
}

export function demarchNextWork(_ctx: IpcContext): Promise<string> {
  return queryKernel('next_work', {});
}

export function demarchRunEvents(
  _ctx: IpcContext,
  limit?: number,
  since?: string,
): Promise<string> {
  const params: Record<string, unknown> = {};
  if (limit) params.limit = limit;
  if (since) params.since = since;
  return queryKernel('run_events', params);
}
