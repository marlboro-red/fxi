import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import * as path from "path";

describe("getSocketPath", () => {
  const originalEnv = { ...process.env };
  const originalGetuid = process.getuid;
  const originalPlatform = process.platform;

  beforeEach(() => {
    delete process.env["XDG_RUNTIME_DIR"];
    vi.resetModules();
  });

  afterEach(() => {
    process.env = { ...originalEnv };
    if (originalGetuid) {
      process.getuid = originalGetuid;
    } else {
      delete (process as any).getuid;
    }
    Object.defineProperty(process, "platform", { value: originalPlatform });
    vi.restoreAllMocks();
  });

  it("uses XDG_RUNTIME_DIR when set", async () => {
    process.env["XDG_RUNTIME_DIR"] = "/run/user/1000";
    const { getSocketPath } = await import("./socket");
    expect(getSocketPath()).toBe(path.join("/run/user/1000", "fxi.sock"));
  });

  it("falls back to ~/.local/run/fxi.sock when no XDG_RUNTIME_DIR", async () => {
    delete process.env["XDG_RUNTIME_DIR"];
    // os.homedir() will return the real home dir in tests
    const os = await import("os");
    const home = os.homedir();
    const { getSocketPath } = await import("./socket");
    expect(getSocketPath()).toBe(path.join(home, ".local", "run", "fxi.sock"));
  });

  it("uses /tmp/fxi-{uid}.sock when no home dir", async () => {
    delete process.env["XDG_RUNTIME_DIR"];
    // Mock os module to return empty string for homedir
    vi.doMock("os", () => ({
      ...vi.importActual("os"),
      homedir: () => "",
    }));
    const { getSocketPath } = await import("./socket");
    const uid = process.getuid?.() ?? 0;
    expect(getSocketPath()).toBe(`/tmp/fxi-${uid}.sock`);
  });

  it("uses uid 0 when process.getuid is unavailable", async () => {
    delete process.env["XDG_RUNTIME_DIR"];
    vi.doMock("os", () => ({
      ...vi.importActual("os"),
      homedir: () => "",
    }));
    // Simulate Windows where getuid doesn't exist
    (process as any).getuid = undefined;
    const { getSocketPath } = await import("./socket");
    expect(getSocketPath()).toBe("/tmp/fxi-0.sock");
  });

  it("returns named pipe with USERNAME on Windows", async () => {
    Object.defineProperty(process, "platform", { value: "win32" });
    process.env["USERNAME"] = "testuser";
    const { getSocketPath } = await import("./socket");
    expect(getSocketPath()).toBe("\\\\.\\pipe\\fxi-testuser");
  });

  it("returns named pipe without USERNAME fallback on Windows", async () => {
    Object.defineProperty(process, "platform", { value: "win32" });
    delete process.env["USERNAME"];
    const { getSocketPath } = await import("./socket");
    expect(getSocketPath()).toBe("\\\\.\\pipe\\fxi");
  });
});
