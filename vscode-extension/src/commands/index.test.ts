import { describe, it, expect, vi, afterEach } from "vitest";
import * as vscode from "vscode";
import { registerIndexCommands } from "./index";
import { DaemonClient } from "../daemon/client";

afterEach(() => vi.restoreAllMocks());

describe("index task lifecycle", () => {
  it("ignores other tasks before reloading the completed index", async () => {
    let command: (() => Promise<void>) | undefined;
    let finished: ((event: any) => void) | undefined;
    let task: any;
    const listener = { dispose: vi.fn() };
    vi.spyOn(vscode.commands, "registerCommand").mockImplementation((_name, handler) => { command = handler; return { dispose() {} }; });
    vi.spyOn(vscode.tasks, "onDidEndTaskProcess").mockImplementation((handler) => { finished = handler; return listener; });
    vi.spyOn(vscode.tasks, "executeTask").mockImplementation(async (value) => { task = value; return {} as vscode.TaskExecution; });
    const originalFolders = vscode.workspace.workspaceFolders;
    Object.defineProperty(vscode.workspace, "workspaceFolders", { configurable: true, value: [{ uri: vscode.Uri.file("/project with spaces") }] });
    try {
      const client = { connected: true, reload: vi.fn().mockResolvedValue({ success: true, message: "loaded" }) };
      registerIndexCommands({ subscriptions: [] } as unknown as vscode.ExtensionContext, client as unknown as DaemonClient);
      await command!();
      expect(task.execution).toBeInstanceOf(vscode.ProcessExecution);
      finished!({ execution: { task: {} }, exitCode: 0 });
      expect(listener.dispose).not.toHaveBeenCalled();
      expect(client.reload).not.toHaveBeenCalled();
      finished!({ execution: { task }, exitCode: 0 });
      expect(listener.dispose).toHaveBeenCalledOnce();
      expect(client.reload).toHaveBeenCalledWith("/project with spaces");
    } finally {
      Object.defineProperty(vscode.workspace, "workspaceFolders", { configurable: true, value: originalFolders });
    }
  });
});
