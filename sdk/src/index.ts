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
import { debugEnabled, log, setDebug } from "./log.js";

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

/** Global SDK configuration, call before creating the first player.
 * `debug: true` turns on SDK console logs and asks the plugin (via hello)
 * to raise its file-log level to debug — process-wide, raise-only (ADR-0001). */
export function configure(opts: { debug?: boolean }): void {
  setDebug(!!opts.debug);
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
          log("conn", `connecting ws://127.0.0.1:${this.port}/ws`);
          const ws = new WebSocket(`ws://127.0.0.1:${this.port}/ws`);
          this.ws = ws;
          ws.onopen = () => {
            log("conn", "open");
            resolve();
          };
          ws.onmessage = (ev) => this.onMessage(String(ev.data));
          ws.onclose = () => this.onClose();
          ws.onerror = () => {
            if (ws.readyState === WebSocket.CONNECTING) {
              log("conn", `connect failed (port ${this.port})`);
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
    // debug → native raises its file-log level to debug (raise-only).
    return this.request<HelloResult>("hello", {
      client: "web-sdk",
      protocol: 1,
      debug: debugEnabled(),
    });
  }

  private onMessage(text: string): void {
    const frame = parseFrame(text);
    if (!frame) return;
    if ("id" in frame) {
      if (!frame.ok)
        log("protocol", `← #${frame.id} error ${frame.error?.code ?? "UNKNOWN"}: ${frame.error?.message ?? ""}`);
      const waiter = this.pending.get(frame.id);
      if (!waiter) return;
      this.pending.delete(frame.id);
      if (frame.ok) waiter.resolve(frame.result ?? {});
      else
        waiter.reject(
          new PluginError(frame.error?.code ?? "UNKNOWN", frame.error?.message ?? ""),
        );
    } else {
      const ev = frame as ServerEvent;
      // stats arrive every second per player — skipped, it would drown the rest.
      if (ev.event !== "stream.stats")
        log("protocol", `← ${ev.event} ${JSON.stringify(ev.params)}`);
      this.dispatchServerEvent(ev);
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
        player?.handleStateEvent(params as unknown as StreamStateEvent);
        break;
      case "stream.select":
        player?.emit("select", params as { streamId: number });
        break;
      case "app.control":
        player?.handleControlEvent(params as unknown as ControlEvent);
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
    log("conn", `closed, retry in ${delay}ms`);
    setTimeout(() => {
      this.ensureConnected()
        .then(() => {
          this.backoffMs = 500;
          log("conn", "reconnected");
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
    // stream.rect fires per frame during drags — logged settled on the player side instead.
    if (method !== "stream.rect")
      log("protocol", `→ ${method} ${JSON.stringify(params)}`);
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

/** Inputs deciding whether the native overlay window is shown. */
export interface VisibilityInputs {
  /** Host page explicitly concealed the overlay (conceal()). */
  concealed: boolean;
  /** Mount element intersects the viewport (IntersectionObserver). */
  domVisible: boolean;
  /** Tab is active AND the page has focus (not covered by another app). */
  pageVisible: boolean;
  /** Mount element is still attached to the document. */
  connected: boolean;
}

/** Overlay visibility mirrors mount-element visibility 1:1. @internal */
export function computeHidden(v: VisibilityInputs): boolean {
  return v.concealed || !v.domVisible || !v.pageVisible || !v.connected;
}

export interface PlayerOptions {
  /** Stream URL: rtsp://, rtsps://, rtmp(s)://, http(s)://…flv|m3u8, udp://(组播/GB28181 PS) */
  url: string;
  /** The placeholder element the native window tracks 1:1. */
  mount: HTMLElement;
  /** Muted state at open; monitoring defaults to muted. */
  muted?: boolean;
  /** Explicit stream id; defaults to an auto-incremented value. */
  streamId?: number;
  /** Native top-bar title (monitoring wall label). Empty = no bar. */
  title?: string;
  /** Native control-bar buttons: "playPause" | "snapshot" | "record" | "speed". Default: all four. */
  controls?: string[];
  /** Speed steps for the speed button (cycles on click). Default [1,2,4,8,16]. */
  speeds?: number[];
  /** When set, control-bar presses are handed to the page instead of the
   * SDK defaults (needed for GB28181: speed/seek go through platform SIP). */
  onControl?: (ev: ControlEvent) => void;
}

/** Native control-bar button press (`app.control`). */
export interface ControlEvent {
  streamId: number;
  button: "playPause" | "snapshot" | "record" | "speed";
  /** Speed button: the newly selected step. */
  value?: number;
}

export type PlayerEventMap = {
  state: StreamStateEvent;
  info: StreamInfo;
  stats: StreamStats;
  updateAvailable: { version: string; url: string };
  /** Single click on the native overlay window. */
  select: { streamId: number };
  /** Native control-bar button press (only when no `onControl` option). */
  control: ControlEvent;
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
  } = { state: new Set(), info: new Set(), stats: new Set(), updateAvailable: new Set(), select: new Set(), control: new Set() };
  private rectObserver?: ResizeObserver;
  private intersection?: IntersectionObserver;
  private scrollRaf = 0;
  private lastRectKey = "";
  private zoomTimer?: number;
  private opened = false;
  // Visibility inputs — the native window is shown iff the mount element is
  // visible in the viewport AND the page is showing it (tab active + focused).
  private concealed = false;
  private domVisible = true;
  private pageVisible = true;
  // Playback state mirrored from stream.state (drives playPause default).
  private paused = false;
  private recording = false;

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

  /** @internal Playback state mirror (stream.state events). */
  handleStateEvent(s: StreamStateEvent): void {
    log("player", `state #${this.streamId} ${s.state}${s.code ? ` (${s.code})` : ""}`);
    if (s.state === "paused") this.paused = true;
    else if (s.state === "playing") this.paused = false;
  }

  /** @internal Control-bar press: page hook first, SDK defaults after. */
  handleControlEvent(ev: ControlEvent): void {
    log("player", `control #${this.streamId} ${ev.button}${ev.value !== undefined ? `=${ev.value}` : ""}`);
    if (this.opts.onControl) {
      this.opts.onControl(ev);
      return;
    }
    this.emit("control", ev);
    switch (ev.button) {
      case "playPause":
        void this.setPaused(!this.paused);
        break;
      case "snapshot":
        void this.snapshot().catch(() => undefined);
        break;
      case "record":
        void (this.recording ? this.recordStop() : this.recordStart()).catch(
          () => undefined,
        );
        break;
      case "speed":
        // Server-paced streams: nothing to do locally — the page hooks this
        // (onControl) and sends the platform SIP command. The native bar
        // already shows the cycled step.
        break;
    }
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
    log("player", `open #${this.streamId} ${this.opts.url}`);
    await conn.ensureConnected();
    await conn.request("stream.open", {
      streamId: this.streamId,
      url: this.opts.url,
      muted: this.opts.muted ?? true,
      rect: computeScreenRect(this.opts.mount),
      title: this.opts.title ?? "",
      controls: this.opts.controls,
      speeds: this.opts.speeds,
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

  /** Pushes the current mount rect (physical px) + visibility to the plugin. */
  syncRect(): void {
    if (!this.opened) return;
    const rect = computeScreenRect(this.opts.mount);
    const hidden = computeHidden({
      concealed: this.concealed,
      domVisible: this.domVisible,
      pageVisible: this.pageVisible,
      connected: this.opts.mount.isConnected,
    });
    const key = `${rect.l},${rect.t},${rect.w},${rect.h}${hidden ? "H" : ""}`;
    if (key === this.lastRectKey) return;
    // Visibility flips log immediately (troubleshooting-critical);
    // position logs wait for the rect to settle (no per-frame spam).
    if (this.lastRectKey.endsWith("H") !== hidden)
      log("rect", `#${this.streamId} ${hidden ? "hidden" : "shown"}`);
    this.scheduleRectLog(rect, hidden);
    this.lastRectKey = key;
    void Connection.get()
      .request("stream.rect", { streamId: this.streamId, rect, hidden })
      .catch(() => undefined);
  }

  private rectLogTimer?: number;

  /** @internal Debug: log the settled rect ~150ms after the last change. */
  private scheduleRectLog(rect: Rect, hidden: boolean): void {
    if (this.rectLogTimer) window.clearTimeout(this.rectLogTimer);
    this.rectLogTimer = window.setTimeout(() => {
      this.rectLogTimer = undefined;
      log("rect", `#${this.streamId} rect ${rect.l},${rect.t} ${rect.w}x${rect.h}${hidden ? " hidden" : ""}`);
    }, 150);
  }

  /** Temporarily hides the native window (e.g. page modal over the video). */
  conceal(): void {
    this.concealed = true;
    this.lastRectKey = "";
    this.syncRect();
  }

  reveal(): void {
    this.concealed = false;
    this.lastRectKey = "";
    this.syncRect();
  }

  setMuted(muted: boolean): Promise<void> {
    return Connection.get()
      .request("stream.mute", { streamId: this.streamId, muted })
      .then(() => undefined);
  }

  /** Display freeze: picture holds, audio silences, stream keeps draining. */
  setPaused(paused: boolean): Promise<void> {
    return Connection.get()
      .request("stream.pause", { streamId: this.streamId, paused })
      .then(() => undefined);
  }

  /** Seeks a seekable source (files/test). GB28181/RTSP-NVR playback:
   * orchestrate close+reopen via the platform instead. */
  seek(timeMs: number): Promise<void> {
    return Connection.get()
      .request("stream.seek", { streamId: this.streamId, timeMs })
      .then(() => undefined);
  }

  /** Playback speed for locally-paced sources (test/seekable). Server-paced
   * streams (GB28181/RTSP) reject with UNSUPPORTED_SPEED — use platform SIP. */
  setSpeed(speed: number): Promise<void> {
    return Connection.get()
      .request("stream.setSpeed", { streamId: this.streamId, speed })
      .then(() => undefined);
  }

  /** Starts codec-copy MP4 recording; resolves to the local file path. */
  recordStart(): Promise<string> {
    return Connection.get()
      .request<{ record?: { path?: string } }>("stream.record.start", {
        streamId: this.streamId,
      })
      .then((r) => {
        const path = r.record?.path;
        if (!path) throw new PluginError("RECORD_FAILED", "no path in response");
        this.recording = true;
        return path;
      });
  }

  recordStop(): Promise<string> {
    return Connection.get()
      .request<{ record?: { path?: string } }>("stream.record.stop", {
        streamId: this.streamId,
      })
      .then((r) => {
        const path = r.record?.path;
        if (!path) throw new PluginError("RECORD_FAILED", "no path in response");
        this.recording = false;
        return path;
      });
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
    log("player", `close #${this.streamId}`);
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
    // Element visibility in the viewport (also false for display:none / fully
    // scrolled out / clipped by ancestors).
    this.intersection = new IntersectionObserver((entries) => {
      this.domVisible = entries[entries.length - 1]?.isIntersecting ?? true;
      this.syncRect();
    });
    this.intersection.observe(this.opts.mount);
    // Page visibility: tab switched away (visibilitychange) or another app
    // covers / takes focus from the browser (blur/focus) → hide the overlay.
    const onPageVisible = () => {
      this.pageVisible = document.visibilityState === "visible" && document.hasFocus();
      this.syncRect();
    };
    this.pageVisible = document.visibilityState === "visible" && document.hasFocus();
    document.addEventListener("visibilitychange", onPageVisible);
    window.addEventListener("blur", onPageVisible);
    window.addEventListener("focus", onPageVisible);
    this.rectObserver = new ResizeObserver(() => this.syncRect());
    this.rectObserver.observe(this.opts.mount);
    window.addEventListener("scroll", onScroll, true);
    window.addEventListener("resize", onGeneric);
    document.addEventListener("fullscreenchange", onGeneric);
    // Safety net poll: browser zoom (devicePixelRatio), missed page-visibility
    // events, and the DOM lifecycle contract — the mount element removed from
    // the document closes the stream, so the native window cannot outlive it.
    this.zoomTimer = window.setInterval(() => {
      if (!this.opts.mount.isConnected) {
        void this.close();
        return;
      }
      onPageVisible();
    }, 500);
    this.removeListeners = () => {
      this.rectObserver?.disconnect();
      this.rectObserver = undefined;
      this.intersection?.disconnect();
      this.intersection = undefined;
      window.removeEventListener("scroll", onScroll, true);
      window.removeEventListener("resize", onGeneric);
      document.removeEventListener("fullscreenchange", onGeneric);
      document.removeEventListener("visibilitychange", onPageVisible);
      window.removeEventListener("blur", onPageVisible);
      window.removeEventListener("focus", onPageVisible);
      window.clearInterval(this.zoomTimer);
      this.zoomTimer = undefined;
    };
    this.syncRect();
  }

  private removeListeners?: () => void;

  private stopRectSync(): void {
    if (this.scrollRaf) cancelAnimationFrame(this.scrollRaf);
    this.scrollRaf = 0;
    if (this.rectLogTimer) window.clearTimeout(this.rectLogTimer);
    this.rectLogTimer = undefined;
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
