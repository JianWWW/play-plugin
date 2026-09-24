import { describe, expect, it } from "vitest";
import {
  DEFAULT_PORTS,
  encodeRequest,
  isSupportedUrl,
  parseFrame,
  PROTOCOL_VERSION,
} from "../src/protocol";

describe("protocol", () => {
  it("encodes requests in the v1 wire shape", () => {
    const raw = encodeRequest(7, "stream.open", { streamId: 42, url: "rtsp://x/y" });
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    expect(parsed).toEqual({
      v: PROTOCOL_VERSION,
      id: 7,
      method: "stream.open",
      params: { streamId: 42, url: "rtsp://x/y" },
    });
  });

  it("parses responses", () => {
    const frame = parseFrame(
      `{"v":1,"id":7,"ok":false,"error":{"code":"LIMIT_STREAMS","message":"max 32"}}`,
    );
    expect(frame).toMatchObject({
      id: 7,
      ok: false,
      error: { code: "LIMIT_STREAMS" },
    });
  });

  it("parses events and routes streamId payloads", () => {
    const frame = parseFrame(
      `{"v":1,"event":"stream.stats","params":{"streamId":42,"fps":29.7,"bitrateKbps":3200,"dropped":0,"decoder":"d3d11va"}}`,
    );
    expect(frame).toMatchObject({ event: "stream.stats", params: { streamId: 42 } });
  });

  it("rejects wrong-version and garbage frames", () => {
    expect(parseFrame(`{"v":2,"id":1,"ok":true}`)).toBeNull();
    expect(parseFrame(`not json`)).toBeNull();
    expect(parseFrame(`{}`)).toBeNull();
  });

  it("validates supported url schemes", () => {
    expect(isSupportedUrl("rtsp://cam/1")).toBe(true);
    expect(isSupportedUrl("rtsps://cam/1")).toBe(true);
    expect(isSupportedUrl("http://cdn/live.flv")).toBe(true);
    expect(isSupportedUrl("https://cdn/live.m3u8")).toBe(true);
    expect(isSupportedUrl("test://pattern")).toBe(true);
    expect(isSupportedUrl("file:///c:/video.mp4")).toBe(false);
    expect(isSupportedUrl("javascript:alert(1)")).toBe(false);
    expect(isSupportedUrl("")).toBe(false);
  });

  it("exposes the default port list", () => {
    expect(DEFAULT_PORTS.length).toBeGreaterThanOrEqual(1);
    expect(DEFAULT_PORTS[0]).toBeGreaterThan(1024);
  });
});
