import { DEFAULT_PORTS, type PluginInfo, PROTOCOL_VERSION } from "./protocol.js";

export type { PluginInfo };

export interface DetectOptions {
  /** Ports to probe; defaults to DEFAULT_PORTS. */
  ports?: number[];
  /** Per-port fetch timeout. */
  timeoutMs?: number;
}

export interface DetectedPlugin extends PluginInfo {
  /** Local port the plugin answered on. */
  port: number;
}

/**
 * Detects the native plugin by probing its local `/info` endpoint.
 * Returns plugin info when installed, `null` otherwise (→ show download UI).
 */
export async function detectPlugin(opts: DetectOptions = {}): Promise<DetectedPlugin | null> {
  const ports = opts.ports ?? DEFAULT_PORTS;
  const timeoutMs = opts.timeoutMs ?? 800;
  for (const port of ports) {
    try {
      const ctrl = new AbortController();
      const timer = setTimeout(() => ctrl.abort(), timeoutMs);
      const res = await fetch(`http://127.0.0.1:${port}/info`, { signal: ctrl.signal });
      clearTimeout(timer);
      if (!res.ok) continue;
      const info = (await res.json()) as PluginInfo;
      if (info.plugin === "PlayPlugin") {
        return { ...info, port };
      }
    } catch {
      // port not answering — try the next one
    }
  }
  return null;
}

/** Whether a detected plugin speaks the protocol version this SDK expects. */
export function isCompatible(info: PluginInfo): boolean {
  return info.protocol === PROTOCOL_VERSION;
}
