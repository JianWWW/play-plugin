/**
 * Debug logging. Off by default; enable before creating the first player:
 *
 * ```ts
 * import { configure } from "playplugin-sdk";
 * configure({ debug: true });
 * ```
 *
 * One switch for the whole page: SDK console logs + the native plugin's file
 * logs (raised via the hello handshake, process-wide, see ADR-0001).
 *
 * Credentials in URLs are redacted here — the single log exit point, so no
 * call site can leak `scheme://user:pass@host`.
 */

let debugOn = false;

/** @internal */
export function setDebug(v: boolean): void {
  debugOn = v;
}

/** @internal */
export function debugEnabled(): boolean {
  return debugOn;
}

const URL_SECRET = /:\/\/[^/@\s]+@/g;

/** @internal Mask `scheme://user:pass@` → `scheme://***@` (mirrors native redact_url). */
export function redactUrl(msg: string): string {
  return msg.replace(URL_SECRET, "://***@");
}

/** @internal Debug log. Silent unless configure({debug:true}) was called. */
export function log(tag: "conn" | "protocol" | "player" | "rect", msg: string): void {
  if (debugOn) console.debug(`[playplugin][${tag}] ${redactUrl(msg)}`);
}
