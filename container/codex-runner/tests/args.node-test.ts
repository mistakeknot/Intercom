import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { buildCodexExecArgs } from '../src/args.ts';

test('passes the resolved model, reasoning effort, and Standard service tier explicitly', () => {
  assert.deepEqual(
    buildCodexExecArgs({
      workDir: '/workspace/group',
      outputFile: '/tmp/codex-output.txt',
      model: 'gpt-6-astra',
      reasoningEffort: 'high',
      serviceTier: 'standard',
    }),
    [
      'exec',
      '--skip-git-repo-check',
      '--ephemeral',
      '--dangerously-bypass-approvals-and-sandbox',
      '-C',
      '/workspace/group',
      '-m',
      'gpt-6-astra',
      '-c',
      'model_reasoning_effort=high',
      '-c',
      'service_tier=default',
      '-o',
      '/tmp/codex-output.txt',
      '-',
    ],
  );
});

test('pins the container to the Astra-capable stable Codex release', () => {
  const dockerfile = readFileSync('container/Dockerfile.codex', 'utf8');
  assert.match(dockerfile, /npm install -g @openai\/codex@0\.153\.2/);
  assert.doesNotMatch(dockerfile, /npm install -g @openai\/codex\s*$/m);
});
