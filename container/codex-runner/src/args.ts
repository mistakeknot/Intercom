export type CodexReasoningEffort = 'low' | 'medium' | 'high' | 'xhigh' | 'max';
export type CodexServiceTier = 'standard' | 'fast' | 'flex' | 'batch';

export interface CodexExecOptions {
  workDir: string;
  outputFile: string;
  model: string;
  reasoningEffort?: CodexReasoningEffort;
  serviceTier?: CodexServiceTier;
}

/** Build an explicit, reproducible Codex invocation from the host-resolved profile. */
export function buildCodexExecArgs(options: CodexExecOptions): string[] {
  const args = [
    'exec',
    '--skip-git-repo-check',
    '--ephemeral',
    '--dangerously-bypass-approvals-and-sandbox',
    '-C',
    options.workDir,
    '-m',
    options.model,
  ];

  if (options.reasoningEffort) {
    args.push('-c', `model_reasoning_effort=${options.reasoningEffort}`);
  }
  if (options.serviceTier) {
    const serviceTier =
      options.serviceTier === 'standard' ? 'default' : options.serviceTier;
    args.push('-c', `service_tier=${serviceTier}`);
  }

  args.push('-o', options.outputFile, '-');
  return args;
}
