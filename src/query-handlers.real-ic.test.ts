/**
 * Runs sprint_phase and spec_lookup against the installed ic binary, in a
 * throwaway intercore database. Skips when ic is not on PATH.
 */
import { execFileSync } from 'child_process';
import fs from 'fs';
import os from 'os';
import path from 'path';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';

import { handleQuery } from './query-handlers.js';

function hasIc(): boolean {
  try {
    execFileSync('which', ['ic'], { stdio: 'ignore' });
    return true;
  } catch {
    return false;
  }
}

function ic(args: string[]): string {
  return execFileSync('ic', args, { encoding: 'utf-8' }).trim();
}

describe.skipIf(!hasIc())('query-handlers against real ic', () => {
  const originalCwd = process.cwd();
  let dir = '';
  let runId = '';
  let artifactId = '';

  beforeAll(() => {
    // ic finds .clavain/intercore.db from the working directory, and
    // `ic run current` uses it as the project.
    dir = fs.realpathSync(
      fs.mkdtempSync(path.join(os.tmpdir(), 'intercom-ic-')),
    );
    process.chdir(dir);
    ic(['init']);
    runId = JSON.parse(
      ic([
        'run',
        'create',
        `--project=${dir}`,
        '--goal=intercom reader test',
        '--json',
      ]),
    ).id;
    fs.writeFileSync(path.join(dir, 'spec.md'), '# spec\n');
    artifactId = JSON.parse(
      ic([
        'run',
        'artifact',
        'add',
        runId,
        '--phase=brainstorm',
        '--path=spec.md',
        '--type=prd',
        '--json',
      ]),
    ).id;
  });

  afterAll(() => {
    process.chdir(originalCwd);
    if (dir) fs.rmSync(dir, { recursive: true, force: true });
  });

  it('sprint_phase returns the active run phase', () => {
    const result = handleQuery('sprint_phase', {}, 'main', true);
    expect(result.status).toBe('ok');
    expect(JSON.parse(result.result)).toEqual({
      run_id: runId,
      phase: ic(['run', 'phase', runId]),
    });
  });

  it('sprint_phase accepts an explicit runId', () => {
    const result = handleQuery('sprint_phase', { runId }, 'main', true);
    expect(result.status).toBe('ok');
    expect(JSON.parse(result.result).run_id).toBe(runId);
  });

  it('spec_lookup lists the active run artifacts', () => {
    const result = handleQuery('spec_lookup', {}, 'main', true);
    expect(result.status).toBe('ok');
    const artifacts = JSON.parse(result.result);
    expect(artifacts).toHaveLength(1);
    expect(artifacts[0]).toMatchObject({
      id: artifactId,
      run_id: runId,
      path: 'spec.md',
      type: 'prd',
    });
  });

  it('spec_lookup selects one artifact by id', () => {
    const result = handleQuery('spec_lookup', { artifactId }, 'main', true);
    expect(result.status).toBe('ok');
    expect(JSON.parse(result.result)).toMatchObject({
      id: artifactId,
      phase: 'brainstorm',
    });
  });

  it('spec_lookup reports an unknown artifact id', () => {
    const result = handleQuery(
      'spec_lookup',
      { artifactId: 'no-such-artifact' },
      'main',
      true,
    );
    expect(result.status).toBe('error');
  });
});
