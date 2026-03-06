/**
 * Shared protocol types and IO helpers for Intercom container agents.
 * All runtimes (Claude, Gemini, Codex) speak this same protocol.
 *
 * Output channel: prefers Unix domain socket (length-prefixed frames)
 * when available at /workspace/ipc/output.sock, falls back to stdout
 * markers for backward compatibility with older host versions.
 */
export interface ContainerInput {
    prompt: string;
    sessionId?: string;
    groupFolder: string;
    chatJid: string;
    isMain: boolean;
    isScheduledTask?: boolean;
    assistantName?: string;
    model?: string;
    secrets?: Record<string, string>;
}
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
export declare const OUTPUT_START_MARKER = "---INTERCOM_OUTPUT_START---";
export declare const OUTPUT_END_MARKER = "---INTERCOM_OUTPUT_END---";
/**
 * Try to connect to the host's UDS output socket.
 * Returns true if connected, false if unavailable (fall back to stdout).
 */
export declare function initUdsOutput(): Promise<boolean>;
/**
 * Close the UDS output socket cleanly.
 */
export declare function closeUdsOutput(): void;
/**
 * Write output to the host. Tries UDS first, falls back to stdout markers.
 */
export declare function writeOutput(output: ContainerOutput): void;
export declare function log(message: string): void;
export declare function readStdin(): Promise<string>;
