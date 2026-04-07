/**
 * Output stream parsing and formatting for container agent communication.
 */

// Sentinel markers for robust output parsing (must match agent-runner)
export const OUTPUT_START_MARKER = '---INTERCOM_OUTPUT_START---';
export const OUTPUT_END_MARKER = '---INTERCOM_OUTPUT_END---';

export interface StreamEvent {
  type: 'tool_start' | 'text_delta';
  toolName?: string;
  toolInput?: string;
  text?: string;
}

export interface ContainerOutput {
  status: 'success' | 'error';
  result: string | null;
  newSessionId?: string;
  error?: string;
  model?: string;
  event?: StreamEvent;
}

/**
 * Parse the last output marker pair from accumulated stdout (legacy mode).
 * Returns the parsed ContainerOutput or throws on parse failure.
 */
export function parseLegacyOutput(stdout: string): ContainerOutput {
  const startIdx = stdout.indexOf(OUTPUT_START_MARKER);
  const endIdx = stdout.indexOf(OUTPUT_END_MARKER);

  let jsonLine: string;
  if (startIdx !== -1 && endIdx !== -1 && endIdx > startIdx) {
    jsonLine = stdout
      .slice(startIdx + OUTPUT_START_MARKER.length, endIdx)
      .trim();
  } else {
    // Fallback: last non-empty line (backwards compatibility)
    const lines = stdout.trim().split('\n');
    jsonLine = lines[lines.length - 1];
  }

  return JSON.parse(jsonLine);
}

/**
 * Incrementally parse OUTPUT_START/END marker pairs from a streaming buffer.
 * Returns parsed outputs and the remaining unparsed buffer.
 */
export function parseStreamChunks(
  buffer: string,
): { outputs: ContainerOutput[]; remaining: string } {
  const outputs: ContainerOutput[] = [];
  let parseBuffer = buffer;

  let startIdx: number;
  while ((startIdx = parseBuffer.indexOf(OUTPUT_START_MARKER)) !== -1) {
    const endIdx = parseBuffer.indexOf(OUTPUT_END_MARKER, startIdx);
    if (endIdx === -1) break; // Incomplete pair, wait for more data

    const jsonStr = parseBuffer
      .slice(startIdx + OUTPUT_START_MARKER.length, endIdx)
      .trim();
    parseBuffer = parseBuffer.slice(endIdx + OUTPUT_END_MARKER.length);

    const parsed: ContainerOutput = JSON.parse(jsonStr);
    outputs.push(parsed);
  }

  return { outputs, remaining: parseBuffer };
}
