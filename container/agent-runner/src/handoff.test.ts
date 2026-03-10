import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import fs from 'fs';
import path from 'path';
import os from 'os';
import {
  writeHandoffNote,
  readHandoffNote,
  reconstructAmbientContext,
  formatResumeContext,
  HandoffNote,
  HANDOFF_MAX_CHARS,
} from './handoff.js';

describe('handoff', () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'handoff-test-'));
  });

  afterEach(() => {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  describe('writeHandoffNote', () => {
    it('writes valid handoff.json atomically', () => {
      const note: HandoffNote = {
        version: 1,
        created_at: new Date().toISOString(),
        source: 'agent',
        session_id: 'test-session',
        task: { summary: 'Testing handoff' },
        decisions: ['Decision 1'],
        pending: ['Next step 1'],
        gotchas: ['Watch out for X'],
      };
      writeHandoffNote(tmpDir, note);

      const written = JSON.parse(fs.readFileSync(path.join(tmpDir, 'handoff.json'), 'utf-8'));
      expect(written.version).toBe(1);
      expect(written.source).toBe('agent');
      expect(written.task.summary).toBe('Testing handoff');
    });

    it('truncates oversized notes', () => {
      const note: HandoffNote = {
        version: 1,
        created_at: new Date().toISOString(),
        source: 'agent',
        session_id: 'test-session',
        task: { summary: 'Testing' },
        decisions: Array.from({ length: 50 }, (_, i) => `Decision ${i}: ${'x'.repeat(100)}`),
        pending: ['Step 1'],
        gotchas: [],
      };
      writeHandoffNote(tmpDir, note);

      const content = fs.readFileSync(path.join(tmpDir, 'handoff.json'), 'utf-8');
      expect(content.length).toBeLessThanOrEqual(HANDOFF_MAX_CHARS);
    });
  });

  describe('readHandoffNote', () => {
    it('returns parsed note when file exists', () => {
      const note: HandoffNote = {
        version: 1,
        created_at: new Date().toISOString(),
        source: 'agent',
        session_id: 'test-session',
        task: { summary: 'Testing' },
        decisions: [],
        pending: [],
        gotchas: [],
      };
      fs.writeFileSync(path.join(tmpDir, 'handoff.json'), JSON.stringify(note));

      const result = readHandoffNote(tmpDir);
      expect(result).not.toBeNull();
      expect(result!.task.summary).toBe('Testing');
    });

    it('consumes file after successful read (one-shot)', () => {
      const note: HandoffNote = {
        version: 1,
        created_at: new Date().toISOString(),
        source: 'agent',
        session_id: 'test-session',
        task: { summary: 'Testing' },
        decisions: [],
        pending: [],
        gotchas: [],
      };
      fs.writeFileSync(path.join(tmpDir, 'handoff.json'), JSON.stringify(note));

      readHandoffNote(tmpDir);
      // File should be renamed to .consumed
      expect(fs.existsSync(path.join(tmpDir, 'handoff.json'))).toBe(false);
      expect(fs.existsSync(path.join(tmpDir, 'handoff.json.consumed'))).toBe(true);
      // Second read returns null
      expect(readHandoffNote(tmpDir)).toBeNull();
    });

    it('returns null when file is missing', () => {
      expect(readHandoffNote(tmpDir)).toBeNull();
    });

    it('returns null for malformed JSON', () => {
      fs.writeFileSync(path.join(tmpDir, 'handoff.json'), '{bad json');
      expect(readHandoffNote(tmpDir)).toBeNull();
    });

    it('returns null for wrong version', () => {
      fs.writeFileSync(path.join(tmpDir, 'handoff.json'), JSON.stringify({
        version: 2, task: { summary: 'test' }, decisions: [], pending: [], gotchas: [],
      }));
      expect(readHandoffNote(tmpDir)).toBeNull();
    });

    it('returns null when decisions is not an array', () => {
      fs.writeFileSync(path.join(tmpDir, 'handoff.json'), JSON.stringify({
        version: 1, task: { summary: 'test' }, decisions: 'not array', pending: [], gotchas: [],
      }));
      expect(readHandoffNote(tmpDir)).toBeNull();
    });
  });

  describe('reconstructAmbientContext', () => {
    it('builds context from conversations directory', () => {
      const convDir = path.join(tmpDir, 'conversations');
      fs.mkdirSync(convDir);
      fs.writeFileSync(path.join(convDir, '2026-03-10-debugging-auth.md'), 'test');

      const ctx = reconstructAmbientContext(tmpDir);
      expect(ctx).toContain('debugging auth');
    });

    it('returns minimal context when nothing is available', () => {
      const ctx = reconstructAmbientContext(tmpDir);
      expect(ctx).toContain('reconstructed');
    });
  });

  describe('formatResumeContext', () => {
    it('formats agent-authored note as markdown', () => {
      const note: HandoffNote = {
        version: 1,
        created_at: '2026-03-10T14:30:00Z',
        source: 'agent',
        session_id: 'abc',
        task: { bead_id: 'Demarch-4wm', summary: 'Building resumption' },
        decisions: ['Use system prompt prepend'],
        pending: ['Implement F2'],
        gotchas: ['Avoid full replay'],
      };
      const result = formatResumeContext(note);
      expect(result).toContain('Previous Session Context');
      expect(result).toContain('Building resumption');
      expect(result).toContain('Use system prompt prepend');
      expect(result).toContain('Implement F2');
      expect(result).toContain('Avoid full replay');
    });
  });
});
