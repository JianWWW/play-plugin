/**
 * PlayPlugin TypeScript SDK.
 *
 * ```ts
 * import { detectPlugin, OverlayPlayer, downloadUrl } from "playplugin-sdk";
 *
 * const info = await detectPlugin();
 * if (!info) { showDownloadBanner(downloadUrl); }
 *
 * const player = new OverlayPlayer({ url: "rtsp://…", mount: document.getElementById("cam1")! });
 * await player.open();
 * player.on("stats", (s) => renderBadge(s.fps, s.decoder));
 * ```
 */

import {
  detectPlugin,
  isCompatible,
  type DetectOptions,
  type DetectedPlugin,
  type PluginInfo,
} from "./detect.js";
import {
  DEFAULT_PORTS,
  encodeRequest,
  isSupportedUrl,
  parseFrame,
  type HelloResult,
  type Rect,
  type ServerEvent,
  type StreamInfo,
  type StreamState,
  type StreamStateEvent,
  type StreamStats,
} from "./protocol.js";
import { computeScreenRect, rectEquals } from "./rect.js";

export { detectPlugin, isCompatible, DEFAULT_PORTS };
export type {
  DetectOptions,
  DetectedPlugin,
  PluginInfo,
  HelloResult,
  Rect,
  ServerEvent,
  StreamInfo,
  StreamState,
  StreamStateEvent,
  StreamStats,
};

/** Where to send users who don't have the plugin (set by the host page). */
export let downloadUrl = "/play-plugin/PlayPlugin.msi";
export function setDownloadUrl(url: string): void {
  downloadUrl = url;
}

type Handler<T> = (payload: T) => void;

/** Error thrown when a request fails at the protocol level. */
export class PluginError extends Error {
  readonly code: string;
  constructor(code: string, message: string) {
    super(`${code}: ${message}`);
    this.code = code;
  }
}

/**
 * One WebSocket per page, shared by all players. Handles hello, heartbeats,
 * reconnection, and re-opening every player's stream after a reconnect
 * (stream state is per-connection on the plugin side).
 */
class Connection {
  private static instance?: Connection;

  static get(): Connection {
    Connection.instance ??= new Connection();
    return Connection.instance;
  }

  private ws?: WebSocket;
  private reqId = 1;
  private port = DEFAULT_PORTS[0] ?? 17653;
  private hello?: Promise<HelloResult>;
  private pending = new Map<
    number,
    { resolve: (r: Record<string, unknown>) => void; reject: (e: PluginError) => void }
  >();
  private players = new Map<number, OverlayPlayer>();
  private backoffMs = 500;

  ensureConnected(): Promise<HelloResult> {
    this.hello ??= this.connectAndHello();
    return this.hello;
  }

  private async connectAndHello(): Promise<HelloResult> {
    try {
      await this.doConnect();
      return await this.requestHello();
    } catch (e) {
      this.hello = undefined; // allow a later retry
      throw e;
    }
  }

  private doConnect(): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      detectPlugin()
        .then((detected: DetectedPlugin | null) => {
          if (!detected) {
            throw new PluginError("NOT_INSTALLED", "plugin not detected");
          }
          this.port = detected.port;
          const ws = new WebSocket(`ws://127.0.0.1:${this.port}/ws`);
          this.ws = ws;
          ws.onopen = () => resolve();
          ws.onmessage = (ev) => this.onMessage(String(ev.data));
          ws.onclose = () => this.onClose();
          ws.onerror = () => {
            if (ws.readyState === WebSocket.CONNECTING) {
              this.ws = undefined;
              reject(
                new PluginError(
                  "CONNECT_FAILED",
                  `cannot reach ws://127.0.0.1:${this.port}/ws`,
                ),
              );
            }
          };
        })
        .catch(reject);
    });
  }

  private requestHello(): Promise<HelloResult> {
    return this.request<HelloResult>("hello", { client: "web-sdk", protocol: 1 });
  }

  private onMessage(text: string): void {
    const frame = parseFrame(text);
    if (!frame) return;
    if ("id" in frame) {
      const waiter = this.pending.get(frame.id);
      if (!waiter) return;
      this.pending.delete(frame.id);
      if (frame.ok) waiter.resolve(frame.result ?? {});
      else
        waiter.reject(
          new PluginError(frame.error?.code ?? "UNKNOWN", frame.error?.message ?? ""),
        );
    } else {
      this.dispatchServerEvent(frame as ServerEvent);
    }
  }

  private dispatchServerEvent(ev: ServerEvent): void {
    const params = ev.params as Record<string, unknown> | null;
    const streamId = params ? Number(params["streamId"]) : NaN;
    const player = Number.isFinite(streamId) ? this.players.get(streamId) : undefined;
    switch (ev.event) {
      case "stream.info":
        player?.emit("info", params as unknown as StreamInfo);
        break;
      case "stream.stats":
        player?.emit("stats", params as unknown as StreamStats);
        break;
      case "stream.state":
        player?.emit("state", params as unknown as StreamStateEvent);
        break;
      case "app.updateAvailable":
        for (const p of this.players.values()) p.emit("updateAvailable", ev.params as { version: string; url: string });
        break;
      default:
        break;
    }
  }

  private onClose(): void {
    const hadConnection = this.ws !== undefined || this.hello !== undefined;
    this.ws = undefined;
    this.hello = undefined;
    if (!hadConnection) return;
    for (const w of this.pending.values())
      w.reject(new PluginError("DISCONNECTED", "connection lost"));
    this.pending.clear();
    // Reconnect with backoff, then re-open every player's stream.
    const delay = this.backoffMs;
    this.backoffMs = Math.min(this.backoffMs * 2, 5000);
    setTimeout(() => {
      this.ensureConnected()
        .then(() => {
          this.backoffMs = 500;
          return Promise.all([...this.players.values()].map((p) => p.respawn()));
        })
        .then(() => undefined)
        .catch(() => this.onClose()); // keep retrying
    }, delay);
  }

  request<R = Record<string, unknown>>(method: string, params: unknown): Promise<R> {
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      return Promise.reject(new PluginError("NOT_CONNECTED", "plugin connection is not open"));
    }
    const id = this.reqId++;
    const promise = new Promise<R>((resolve, reject) => {
      this.pending.set(id, {
        resolve: resolve as (r: Record<string, unknown>) => void,
        reject,
      });
      setTimeout(() => {
        if (this.pending.has(id)) {
          this.pending.delete(id);
          reject(new PluginError("TIMEOUT", `${method} timed out`));
        }
      }, 10_000);
    });
    ws.send(encodeRequest(id, method, params));
    return promise;
  }

  register(player: OverlayPlayer): void {
    this.players.set(player.streamId, player);
  }

  unregister(player: OverlayPlayer): void {
    this.players.delete(player.streamId);
  }
}

export interface PlayerOptions {
  /** Stream URL: rtsp://, rtsps://, http(s)://…flv, http(s)://…m3u8 */
  url: string;
  /** The placeholder element the native window tracks 1:1. */
  mount: HTMLElement;
  /** Muted state at open; monitoring defaults to muted. */
  muted?: boolean;
  /** Explicit stream id; defaults to an auto-incremented value. */
  streamId?: number;
}

export type PlayerEventMap = {
  state: StreamStateEvent;
  info: StreamInfo;
  stats: StreamStats;
  updateAvailable: { version: string; url: string };
};

/**
 * One native overlay video. The `mount` element stays in normal document flow;
 * the plugin renders into a native window glued to its rect.
 */
export class OverlayPlayer {
  readonly streamId: number;
  private readonly opts: PlayerOptions;
  private readonly handlers: {
    [K in keyof PlayerEventMap]: Set<Handler<PlayerEventMap[K]>>;
  } = { state: new Set(), info: new Set(), stats: new Set(), updateAvailable: new Set() };
  private rectObserver?: ResizeObserver;
  private scrollRaf = 0;
  private lastRectKey = "";
  private zoomTimer?: number;
  private opened = false;

  constructor(opts: PlayerOptions) {
    if (!isSupportedUrl(opts.url)) {
      throw new Error(`unsupported url: ${opts.url}`);
    }
    this.opts = opts;
    this.streamId = opts.streamId ?? OverlayPlayer.nextId();
  }

  private static nextSource = 1;
  private static nextId(): number {
    return OverlayPlayer.nextSource++;
  }

  on<K extends keyof PlayerEventMap>(event: K, handler: Handler<PlayerEventMap[K]>): void {
    this.handlers[event].add(handler);
  }

  off<K extends keyof PlayerEventMap>(event: K, handler: Handler<PlayerEventMap[K]>): void {
    this.handlers[event].delete(handler);
  }

  /** @internal */
  emit<K extends keyof PlayerEventMap>(event: K, payload: PlayerEventMap[K]): void {
    for (const h of this.handlers[event]) h(payload);
  }

  /** Connects (shared), opens the stream and starts tracking the mount rect. */
  async open(): Promise<void> {
    const conn = Connection.get();
    await conn.ensureConnected();
    await conn.request("stream.open", {
      streamId: this.streamId,
      url: this.opts.url,
      muted: this.opts.muted ?? true,
      rect: computeScreenRect(this.opts.mount),
    });
    conn.register(this);
    this.opened = true;
    this.startRectSync();
  }

  /** Re-opens after a plugin reconnect. */
  async respawn(): Promise<void> {
    if (!this.opened) return;
    this.stopRectSync();
    this.lastRectKey = "";
    await this.open();
  }

  /** Pushes the current mount rect (physical px) to the plugin. */
  syncRect(hidden = false): void {
    if (!this.opened) return;
    const rect = computeScreenRect(this.opts.mount);
    const key = `${rect.l},${rect.t},${rect.w},${rect.h}${hidden ? "H" : ""}`;
    if (key === this.lastRectKey) return;
    this.lastRectKey = key;
    void Connection.get()
      .request("stream.rect", { streamId: this.streamId, rect, hidden })
      .catch(() => undefined);
  }

  /** Temporarily hides the native window (e.g. page modal over the video). */
  conceal(): void {
    this.lastRectKey = "";
    this.syncRect(true);
  }

  reveal(): void {
    this.lastRectKey = "";
    this.syncRect(false);
  }

  setMuted(muted: boolean): Promise<void> {
    return Connection.get()
      .request("stream.mute", { streamId: this.streamId, muted })
      .then(() => undefined);
  }

  /** Saves the current frame as PNG; resolves to the local file path. */
  snapshot(): Promise<string> {
    return Connection.get()
      .request<{ snapshot?: { path?: string } }>("stream.snapshot", {
        streamId: this.streamId,
      })
      .then((r) => {
        const path = r.snapshot?.path;
        if (!path) throw new PluginError("SNAPSHOT_FAILED", "no path in response");
        return path;
      });
  }

  /** Closes the stream and stops tracking. Safe to call twice. */
  async close(): Promise<void> {
    if (!this.opened) return;
    this.opened = false;
    this.stopRectSync();
    Connection.get().unregister(this);
    await Connection.get()
      .request("stream.close", { streamId: this.streamId })
      .catch(() => undefined);
  }

  private startRectSync(): void {
    const onGeneric = () => this.syncRect();
    const onScroll = () => {
      // Coalesce scroll storms into animation frames.
      if (!this.scrollRaf) {
        this.scrollRaf = requestAnimationFrame(() => {
          this.scrollRaf = 0;
          this.syncRect();
        });
      }
    };
    this.rectObserver = new ResizeObserver(() => this.syncRect());
    this.rectObserver.observe(this.opts.mount);
    window.addEventListener("scroll", onScroll, true);
    window.addEventListener("resize", onGeneric);
    document.addEventListener("fullscreenchange", onGeneric);
    // Browser zoom changes devicePixelRatio; poll lightly as a safety net.
    this.zoomTimer = window.setInterval(() => this.syncRect(), 500);
    this.removeListeners = () => {
      this.rectObserver?.disconnect();
      this.rectObserver = undefined;
      window.removeEventListener("scroll", onScroll, true);
      window.removeEventListener("resize", onGeneric);
      document.removeEventListener("fullscreenchange", onGeneric);
      window.clearInterval(this.zoomTimer);
      this.zoomTimer = undefined;
    };
    this.syncRect();
  }

  private removeListeners?: () => void;

  private stopRectSync(): void {
    if (this.scrollRaf) cancelAnimationFrame(this.scrollRaf);
    this.scrollRaf = 0;
    this.removeListeners?.();
    this.removeListeners = undefined;
  }
}

/** Convenience: detect + notify the host page when the plugin is missing. */
export async function ensurePlugin(
  onMissing?: () => void,
  opts?: DetectOptions,
): Promise<PluginInfo | null> {
  const info = await detectPlugin(opts);
  if (!info || !isCompatible(info)) {
    onMissing?.();
    return null;
  }
  return info;
}
