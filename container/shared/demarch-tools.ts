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

export function demarchResearch(_ctx: IpcContext, query: string): Promise<string> {
  return queryKernel('research', { query });
}

// --- Write operations (H2) ---

export function demarchCreateIssue(
  _ctx: IpcContext,
  title: string,
  description?: string,
  priority?: string,
  issueType?: string,
  labels?: string[],
): Promise<string> {
  const params: Record<string, unknown> = { title };
  if (description) params.description = description;
  if (priority) params.priority = priority;
  if (issueType) params.issue_type = issueType;
  if (labels) params.labels = labels;
  return queryKernel('create_issue', params);
}

export function demarchUpdateIssue(
  _ctx: IpcContext,
  id: string,
  status?: string,
  priority?: string,
  title?: string,
  description?: string,
  notes?: string,
): Promise<string> {
  const params: Record<string, unknown> = { id };
  if (status) params.status = status;
  if (priority) params.priority = priority;
  if (title) params.title = title;
  if (description) params.description = description;
  if (notes) params.notes = notes;
  return queryKernel('update_issue', params);
}

export function demarchCloseIssue(
  _ctx: IpcContext,
  id: string,
  reason?: string,
): Promise<string> {
  const params: Record<string, unknown> = { id };
  if (reason) params.reason = reason;
  return queryKernel('close_issue', params);
}

export function demarchStartRun(
  _ctx: IpcContext,
  title?: string,
  description?: string,
): Promise<string> {
  const params: Record<string, unknown> = {};
  if (title) params.title = title;
  if (description) params.description = description;
  return queryKernel('start_run', params);
}

export function demarchApproveGate(
  _ctx: IpcContext,
  gateId: string,
  reason?: string,
): Promise<string> {
  const params: Record<string, unknown> = { gate_id: gateId };
  if (reason) params.reason = reason;
  return queryKernel('approve_gate', params);
}
