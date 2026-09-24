/** Wire protocol v1 — mirrors crates/server/src/protocol.rs. */

export const PROTOCOL_VERSION = 1;

/** Preferred local ports, in probe order. */
export const DEFAULT_PORTS = [17653, 17654, 17655, 17656];

export interface Rect {
  /** Physical pixels, screen coordinates. */
  l: number;
  t: number;
  w: number;
  h: number;
}

export type StreamState =
  | "connecting"
  | "playing"
  | "reconnecting"
  | "stopped"
  | "error";

export interface PluginInfo {
  plugin: string;
  version: string;
  protocol: number;
}

export interface HelloResult {
  plugin: string;
  version: string;
  protocol: number;
  maxStreams: number;
  capabilities: {
    h264: boolean;
    h265: boolean;
    rtsp: boolean;
    flv: boolean;
    hls: boolean;
    hardwareDecode: boolean;
  };
}

export interface StreamInfo {
  streamId: number;
  codec: string;
  width: number;
  height: number;
  /** "d3d11va" | "sw" | "test" | … */
  decoder: string;
}

export interface StreamStats {
  streamId: number;
  fps: number;
  bitrateKbps: number;
  dropped: number;
  decoder: string;
}

export interface StreamStateEvent {
  streamId: number;
  state: StreamState;
  code?: string;
  message?: string;
}

export interface Request {
  v: number;
  id: number;
  method: string;
  params: unknown;
}

export interface Response {
  v: number;
  id: number;
  ok: boolean;
  result?: Record<string, unknown>;
  error?: { code: string; message: string };
}

export interface ServerEvent {
  v: number;
  event: string;
  params: unknown;
}

export function encodeRequest(id: number, method: string, params: unknown): string {
  const req: Request = { v: PROTOCOL_VERSION, id, method, params };
  return JSON.stringify(req);
}

export function parseFrame(text: string): Response | ServerEvent | null {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    return null;
  }
  if (typeof v !== "object" || v === null) return null;
  const obj = v as Record<string, unknown>;
  if (obj["v"] !== PROTOCOL_VERSION) return null;
  if ("id" in obj && "ok" in obj) return obj as unknown as Response;
  if ("event" in obj) return obj as unknown as ServerEvent;
  return null;
}

export function isSupportedUrl(url: string): boolean {
  return /^(rtsp|rtsps|https?|test):\/\//i.test(url);
}
