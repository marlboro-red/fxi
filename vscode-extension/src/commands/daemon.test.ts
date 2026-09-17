import { describe, it, expect, vi, afterEach } from "vitest";
import * as vscode from "vscode";
import { execFile } from "child_process";
import { registerDaemonCommands } from "./daemon";
import { DaemonClient } from "../daemon/client";
vi.mock("child_process", () => ({ execFile: vi.fn() }));
afterEach(() => { vi.restoreAllMocks(); vi.useRealTimers(); });

describe("daemon startup", () => {
  it("passes an executable path and arguments without shell interpolation", async () => {
    vi.useFakeTimers();
    const handlers = new Map<string, (...args: any[]) => any>();
    vi.spyOn(vscode.commands, "registerCommand").mockImplementation((name, handler) => { handlers.set(name, handler); return { dispose() {} }; });
    vi.spyOn(vscode.workspace, "getConfiguration").mockReturnValue({ get: () => "C:\\Program Files\\fxi.exe" } as unknown as vscode.WorkspaceConfiguration);
    vi.mocked(execFile).mockImplementation(((...args: any[]) => { args[3](null); return {}; }) as any);
    const client = { connected: true, connect: vi.fn() };
    registerDaemonCommands({ subscriptions: [] } as unknown as vscode.ExtensionContext, client as unknown as DaemonClient);
    const started = handlers.get("fxi.startDaemon")!();
    await vi.runAllTimersAsync();
    await started;
    expect(execFile).toHaveBeenCalledWith("C:\\Program Files\\fxi.exe", ["daemon", "start", "--watch"], { timeout: 15000 }, expect.any(Function));
  });
});
