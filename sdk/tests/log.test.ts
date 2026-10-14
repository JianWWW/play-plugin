import { describe, it, expect, vi, beforeEach } from "vitest";
import { setDebug, log, redactUrl } from "../src/log.js";

describe("log", () => {
  beforeEach(() => setDebug(false));

  it("is silent unless debug is configured", () => {
    const dbg = vi.spyOn(console, "debug").mockImplementation(() => {});
    log("conn", "hello");
    expect(dbg).not.toHaveBeenCalled();
    setDebug(true);
    log("conn", "hello");
    expect(dbg).toHaveBeenCalledWith("[playplugin][conn] hello");
    dbg.mockRestore();
  });

  it("redacts URL credentials at the log exit", () => {
    setDebug(true);
    const dbg = vi.spyOn(console, "debug").mockImplementation(() => {});
    log("player", "open #1 rtsp://admin:secret@cam.local:554/ch1");
    expect(dbg).toHaveBeenCalledWith("[playplugin][player] open #1 rtsp://***@cam.local:554/ch1");
    // multiple secrets in one line
    log("conn", "a rtsp://u:p@h1/x b http://u@h2/y");
    expect(dbg).toHaveBeenCalledWith("[playplugin][conn] a rtsp://***@h1/x b http://***@h2/y");
    dbg.mockRestore();
  });

  it("redact helper keeps credential-free URLs untouched", () => {
    expect(redactUrl("rtsp://cam.local/stream")).toBe("rtsp://cam.local/stream");
    expect(redactUrl("rtsp://a:b@cam/stream")).toBe("rtsp://***@cam/stream");
  });
});
