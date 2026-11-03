// Canonical OUP projection/envelope v2. Mirrors EnvelopeV2 / PayloadV2 in
// octos-core/src/ui_protocol.rs and API spec §14. Live delivery is unconditional;
// projection.envelope.v2 opts into the additional session/hydrate replay fields.

export type ThreadId = string;
export type MessageId = string;
export type IsoTimestamp = string;
export type Seq = number;
export type ClientMessageId = string;

export interface UiCursor { stream: string; seq: number }
export interface EnvelopeTokenUsage {
  input_tokens?: number;
  output_tokens?: number;
  reasoning_tokens?: number;
  cache_read_tokens?: number;
  cache_write_tokens?: number;
}
export interface MessageMeta {
  message_id: MessageId;
  persisted_at: IsoTimestamp;
  media?: string[];
}
export interface FileRef { path: string; mime: string; size_bytes: number }
export type EnvelopeToolEndStatus = 'complete' | 'error' | 'skipped' | 'aborted';
export type TurnTerminalOutcome = 'completed' | 'errored' | 'interrupted' | 'rate_limited';
export interface AttachmentOwnerV2 {
  assistant_segment_id?: string | null;
  tool_call_id?: string | null;
}
export interface TurnTerminalError { code: string; message: string; data?: unknown }

interface UserMessagePayload {
  type: 'user_message';
  data: { text: string; files?: FileRef[] };
}
interface AssistantDeltaPayload {
  type: 'assistant_delta';
  data: { text: string; assistant_segment_id: string };
}
interface ReasoningDeltaPayload { type: 'reasoning_delta'; data: { text: string } }
interface AssistantPersistedPayload {
  type: 'assistant_persisted';
  data: { text: string; assistant_segment_id: string; meta: MessageMeta };
}
interface ToolStartPayload {
  type: 'tool_start';
  data: { tool_call_id: string; name: string; arguments_preview?: string | null };
}
interface ToolProgressPayload {
  type: 'tool_progress';
  data: { tool_call_id: string; message: string };
}
interface ToolEndPayload {
  type: 'tool_end';
  data: {
    tool_call_id: string;
    status: EnvelopeToolEndStatus;
    error?: string | null;
    reason?: string | null;
    output_preview?: string | null;
    duration_ms?: number | null;
  };
}
interface FileAttachedPayload {
  type: 'file_attached';
  data: FileRef & { attachment_owner: AttachmentOwnerV2 };
}
interface TurnTerminalPayload {
  type: 'turn_terminal';
  data: {
    outcome: TurnTerminalOutcome;
    error?: TurnTerminalError | null;
    token_usage?: EnvelopeTokenUsage | null;
  };
}
interface BackgroundChildCompletedPayload {
  type: 'background/spawn_complete';
  data: {
    parent_turn_id: string;
    response_to_client_message_id?: string | null;
    task_id: string;
    content: string;
    tool_call_id?: string | null;
    message_id: string;
    source: string;
    persisted_at: string;
    media?: string[];
  };
}

export type PayloadV2 = UserMessagePayload | AssistantDeltaPayload | ReasoningDeltaPayload
  | AssistantPersistedPayload | ToolStartPayload | ToolProgressPayload | ToolEndPayload
  | FileAttachedPayload | TurnTerminalPayload | BackgroundChildCompletedPayload;
export type Payload = PayloadV2;

/** Bare hydrate envelope. Live notification params add routing fields below. */
export interface EnvelopeV2 {
  thread_id: ThreadId;
  turn_id: string;
  seq: Seq;
  cursor?: UiCursor | null;
  client_message_id?: ClientMessageId | null;
  payload: PayloadV2;
}
export interface Envelope extends EnvelopeV2 {
  // Current servers always send session_id. Old fixtures can omit routing.
  session_id?: string;
  topic?: string | null;
}

export const UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2 = 'projection.envelope.v2';

export function isUserMessage(p: Payload): p is UserMessagePayload { return p.type === 'user_message'; }
export function isAssistantDelta(p: Payload): p is AssistantDeltaPayload { return p.type === 'assistant_delta'; }
export function isAssistantPersisted(p: Payload): p is AssistantPersistedPayload { return p.type === 'assistant_persisted'; }
export function isToolStart(p: Payload): p is ToolStartPayload { return p.type === 'tool_start'; }
export function isToolProgress(p: Payload): p is ToolProgressPayload { return p.type === 'tool_progress'; }
export function isToolEnd(p: Payload): p is ToolEndPayload { return p.type === 'tool_end'; }
export function isFileAttached(p: Payload): p is FileAttachedPayload { return p.type === 'file_attached'; }
export function isTurnTerminal(p: Payload): p is TurnTerminalPayload { return p.type === 'turn_terminal'; }
export function isBackgroundChildCompleted(p: Payload): p is BackgroundChildCompletedPayload {
  return p.type === 'background/spawn_complete';
}

// Runtime validation matches required fields and serde's optional/default rules.
// Unknown additive fields are deliberately ignored.
export function isRecord(v: unknown): v is Record<string, unknown> {
  return v !== null && typeof v === 'object' && !Array.isArray(v);
}
const string = (v: unknown): v is string => typeof v === 'string';
const uint = (v: unknown): v is number => typeof v === 'number' && Number.isSafeInteger(v) && v >= 0;
const strings = (v: unknown): boolean => Array.isArray(v) && v.every(string);
const optional = (v: unknown, check: (v: unknown) => boolean): boolean => v == null || check(v);
const file = (v: unknown): boolean => isRecord(v) && string(v.path) && string(v.mime) && uint(v.size_bytes);
const usage = (v: unknown): boolean => isRecord(v) && [
  'input_tokens', 'output_tokens', 'reasoning_tokens', 'cache_read_tokens', 'cache_write_tokens',
].every(key => v[key] === undefined || uint(v[key]));

export function isPayloadV2(value: unknown): value is PayloadV2 {
  if (!isRecord(value) || !isRecord(value.data)) return false;
  const d = value.data;
  switch (value.type) {
    case 'user_message':
      return string(d.text) && (d.files === undefined || (Array.isArray(d.files) && d.files.every(file)));
    case 'assistant_delta':
      return string(d.text) && string(d.assistant_segment_id);
    case 'reasoning_delta': return string(d.text);
    case 'assistant_persisted':
      return string(d.text) && string(d.assistant_segment_id) && isRecord(d.meta)
        && string(d.meta.message_id) && string(d.meta.persisted_at)
        && (d.meta.media === undefined || strings(d.meta.media));
    case 'tool_start':
      return string(d.tool_call_id) && string(d.name) && optional(d.arguments_preview, string);
    case 'tool_progress': return string(d.tool_call_id) && string(d.message);
    case 'tool_end':
      return string(d.tool_call_id) && ['complete', 'error', 'skipped', 'aborted'].includes(d.status as string)
        && optional(d.error, string) && optional(d.reason, string)
        && optional(d.output_preview, string) && optional(d.duration_ms, uint);
    case 'file_attached':
      return file(d) && isRecord(d.attachment_owner)
        && optional(d.attachment_owner.assistant_segment_id, string)
        && optional(d.attachment_owner.tool_call_id, string);
    case 'turn_terminal':
      return ['completed', 'errored', 'interrupted', 'rate_limited'].includes(d.outcome as string)
        && optional(d.error, e => isRecord(e) && string(e.code) && string(e.message))
        && optional(d.token_usage, usage);
    case 'background/spawn_complete':
      return ['parent_turn_id', 'task_id', 'content', 'message_id', 'source', 'persisted_at'].every(key => string(d[key]))
        && optional(d.response_to_client_message_id, string) && optional(d.tool_call_id, string)
        && (d.media === undefined || strings(d.media));
    default: return false;
  }
}
