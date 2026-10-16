import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { handleQuery, type QueryResponse } from './query-handlers.js';
import { execFileSync } from 'child_process';

vi.mock('child_process', () => ({
  execFileSync: vi.fn(),
}));

const mockExecFileSync = vi.mocked(execFileSync);

describe('query-handlers', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  describe('handleQuery routing', () => {
    it('returns error for unknown query type', () => {
      const result = handleQuery('unknown_type', {}, 'main', true);
      expect(result.status).toBe('error');
      expect(result.result).toContain('Unknown query type');
    });
  });

  describe('run_status handler', () => {
    it('calls ic run current --json when no runId', () => {
      // First call: `which ic` succeeds
      mockExecFileSync.mockImplementation((bin: string, args?: readonly string[]) => {
        if (bin === 'which') return '/usr/local/bin/ic';
        if (bin === 'ic') return '{"id":"run-1","phase":"executing","status":"active"}';
        return '';
      });

      const result = handleQuery('run_status', {}, 'main', true);
      expect(result.status).toBe('ok');
      expect(result.result).toContain('run-1');
    });

    it('calls ic run status <id> --json when runId provided', () => {
      mockExecFileSync.mockImplementation((bin: string, args?: readonly string[]) => {
        if (bin === 'which') return '/usr/local/bin/ic';
        if (bin === 'ic') {
          const argList = args as string[];
          expect(argList).toContain('run-42');
          return '{"id":"run-42","status":"completed"}';
        }
        return '';
      });

      const result = handleQuery('run_status', { runId: 'run-42' }, 'main', true);
      expect(result.status).toBe('ok');
      expect(result.result).toContain('run-42');
    });

    it('returns standalone message when ic is not available', () => {
      mockExecFileSync.mockImplementation(() => {
        throw new Error('not found');
      });

      const result = handleQuery('run_status', {}, 'main', true);
      expect(result.status).toBe('error');
      expect(result.result).toContain('standalone mode');
    });
  });

  // Stubs below mirror real ic output: `run current --json` emits
  // {found, id, phase, goal}; `run phase <id>` prints the bare phase name;
  // `run artifact list <id> --json` emits an array of artifacts.
  describe('sprint_phase handler', () => {
    it('resolves the active run, then reads its phase', () => {
      const calls: string[][] = [];
      mockExecFileSync.mockImplementation((bin: string, args?: readonly string[]) => {
        if (bin === 'which') return '/usr/local/bin/ic';
        const argList = [...(args as string[])];
        calls.push(argList);
        if (argList.join(' ') === 'run current --json') {
          return '{"found":true,"goal":"g","id":"run-1","phase":"executing"}';
        }
        if (argList.join(' ') === 'run phase run-1') return 'executing\n';
        throw new Error(`unexpected ic ${argList.join(' ')}`);
      });

      const result = handleQuery('sprint_phase', {}, 'main', true);
      expect(result.status).toBe('ok');
      expect(JSON.parse(result.result)).toEqual({ run_id: 'run-1', phase: 'executing' });
      expect(calls).toEqual([['run', 'current', '--json'], ['run', 'phase', 'run-1']]);
    });

    it('uses runId without resolving the active run', () => {
      mockExecFileSync.mockImplementation((bin: string, args?: readonly string[]) => {
        if (bin === 'which') return '/usr/local/bin/ic';
        expect(args).toEqual(['run', 'phase', 'run-9']);
        return 'review';
      });

      const result = handleQuery('sprint_phase', { runId: 'run-9' }, 'main', true);
      expect(result.status).toBe('ok');
      expect(JSON.parse(result.result)).toEqual({ run_id: 'run-9', phase: 'review' });
    });

    it('returns an error when there is no active run', () => {
      mockExecFileSync.mockImplementation((bin: string) => {
        if (bin === 'which') return '/usr/local/bin/ic';
        // ic exits 1 after printing {"found":false}; execFileSync throws.
        throw new Error('Command failed: ic run current --json');
      });

      const result = handleQuery('sprint_phase', {}, 'main', true);
      expect(result.status).toBe('error');
    });
  });

  describe('spec_lookup handler', () => {
    const artifacts = '[{"id":"art-1","run_id":"run-1","phase":"brainstorm","path":"spec.md","type":"prd"},' +
      '{"id":"art-2","run_id":"run-1","phase":"strategy","path":"plan.md","type":"plan"}]';

    function stubIc() {
      mockExecFileSync.mockImplementation((bin: string, args?: readonly string[]) => {
        if (bin === 'which') return '/usr/local/bin/ic';
        const joined = (args as string[]).join(' ');
        if (joined === 'run current --json') return '{"found":true,"goal":"g","id":"run-1","phase":"executing"}';
        if (joined === 'run artifact list run-1 --json') return artifacts;
        throw new Error(`unexpected ic ${joined}`);
      });
    }

    it('lists artifacts for the active run', () => {
      stubIc();
      const result = handleQuery('spec_lookup', {}, 'main', true);
      expect(result.status).toBe('ok');
      expect(JSON.parse(result.result)).toHaveLength(2);
    });

    it('selects one artifact by id from the run list', () => {
      stubIc();
      const result = handleQuery('spec_lookup', { artifactId: 'art-2' }, 'main', true);
      expect(result.status).toBe('ok');
      expect(JSON.parse(result.result)).toMatchObject({ id: 'art-2', path: 'plan.md' });
    });

    it('reports an unknown artifact id as not found', () => {
      stubIc();
      const result = handleQuery('spec_lookup', { artifactId: 'missing' }, 'main', true);
      expect(result.status).toBe('error');
      expect(result.result).toContain('missing');
    });

    it('never calls the nonexistent ic run artifact get', () => {
      stubIc();
      handleQuery('spec_lookup', { artifactId: 'art-1' }, 'main', true);
      for (const call of mockExecFileSync.mock.calls) {
        expect((call[1] as string[]).join(' ')).not.toContain('artifact get');
      }
    });
  });

  describe('search_beads handler', () => {
    it('searches by status', () => {
      mockExecFileSync.mockImplementation((bin: string, args?: readonly string[]) => {
        if (bin === 'which') return '/usr/local/bin/bd';
        if (bin === 'bd') {
          const argList = args as string[];
          expect(argList).toContain('--status=open');
          return '[{"id":"beads-1","title":"Fix bug"}]';
        }
        return '';
      });

      const result = handleQuery('search_beads', { status: 'open' }, 'main', true);
      expect(result.status).toBe('ok');
      expect(result.result).toContain('beads-1');
    });

    it('looks up specific bead by id', () => {
      mockExecFileSync.mockImplementation((bin: string, args?: readonly string[]) => {
        if (bin === 'which') return '/usr/local/bin/bd';
        if (bin === 'bd') {
          const argList = args as string[];
          expect(argList).toContain('show');
          expect(argList).toContain('beads-abc');
          return '{"id":"beads-abc","title":"Specific bead"}';
        }
        return '';
      });

      const result = handleQuery('search_beads', { id: 'beads-abc' }, 'main', true);
      expect(result.status).toBe('ok');
      expect(result.result).toContain('beads-abc');
    });

    it('returns standalone message when bd not available', () => {
      mockExecFileSync.mockImplementation(() => { throw new Error('not found'); });

      const result = handleQuery('search_beads', {}, 'main', true);
      expect(result.status).toBe('error');
      expect(result.result).toContain('standalone mode');
    });
  });

  describe('next_work handler', () => {
    it('returns ready beads', () => {
      mockExecFileSync.mockImplementation((bin: string) => {
        if (bin === 'which') return '/usr/local/bin/bd';
        return '[{"id":"beads-2","title":"Ready task"}]';
      });

      const result = handleQuery('next_work', {}, 'main', true);
      expect(result.status).toBe('ok');
      expect(result.result).toContain('beads-2');
    });
  });

  describe('run_events handler', () => {
    it('passes limit and since params', () => {
      mockExecFileSync.mockImplementation((bin: string, args?: readonly string[]) => {
        if (bin === 'which') return '/usr/local/bin/ic';
        if (bin === 'ic') {
          const argList = args as string[];
          expect(argList).toContain('--limit=5');
          expect(argList).toContain('--since=2026-02-20T00:00:00Z');
          return '[{"event_type":"phase_change"}]';
        }
        return '';
      });

      const result = handleQuery('run_events', { limit: 5, since: '2026-02-20T00:00:00Z' }, 'main', true);
      expect(result.status).toBe('ok');
      expect(result.result).toContain('phase_change');
    });
  });

  describe('review_summary handler', () => {
    it('returns error when no verdict files exist', () => {
      const result = handleQuery('review_summary', {}, 'main', true);
      expect(result.status).toBe('error');
      expect(result.result).toContain('No review verdicts found');
    });
  });

  describe('graceful degradation', () => {
    it('all ic-dependent handlers return standalone message when ic unavailable', () => {
      mockExecFileSync.mockImplementation(() => { throw new Error('not found'); });

      const icTypes = ['run_status', 'sprint_phase', 'spec_lookup', 'run_events'];
      for (const type of icTypes) {
        const result = handleQuery(type, {}, 'main', true);
        expect(result.status).toBe('error');
        expect(result.result).toContain('standalone mode');
      }
    });

    it('all bd-dependent handlers return standalone message when bd unavailable', () => {
      mockExecFileSync.mockImplementation(() => { throw new Error('not found'); });

      const bdTypes = ['search_beads', 'next_work'];
      for (const type of bdTypes) {
        const result = handleQuery(type, {}, 'main', true);
        expect(result.status).toBe('error');
        expect(result.result).toContain('standalone mode');
      }
    });
  });
});
