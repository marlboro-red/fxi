import { describe, it, expect, vi, afterEach } from "vitest";
import { EventEmitter } from "events";
import * as vscode from "vscode";
import { SearchPanelProvider } from "./SearchPanelProvider";
import type { DaemonClient } from "../daemon/client";

function deferred() {
  let resolve!: (value: any) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<any>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function setup() {
  (vscode.workspace as any).workspaceFolders = [{ uri: { fsPath: "/repo/src" } }];
  const client = Object.assign(new EventEmitter(), { connected: true, contentSearch: vi.fn() });
  const provider = new SearchPanelProvider(vscode.Uri.file("/extension"), client as unknown as DaemonClient);
  let receive!: (msg: any) => void;
  const postMessage = vi.fn();
  provider.resolveWebviewView({
    webview: { postMessage, onDidReceiveMessage: (fn: any) => { receive = fn; } },
    onDidChangeVisibility: vi.fn(),
  } as any, {} as any, {} as any);
  return { provider, client, postMessage, receive };
}
const search = (query: string) => ({ command: "search", query, limit: 20, contextLines: 0, filesOnly: false });
const response = (path: string) => ({ matches: [{ path, line_number: 1 }], duration_ms: 1, files_with_matches: 1, resolved_root: "/repo" });
afterEach(() => { vi.restoreAllMocks(); (vscode.workspace as any).workspaceFolders = undefined; });

describe("SearchPanelProvider", () => {
  it.each([false, true])("ignores an obsolete response, including errors: %s", async (oldFails) => {
    const h = setup(); const older = deferred(); const newer = deferred();
    h.client.contentSearch.mockReturnValueOnce(older.promise).mockReturnValueOnce(newer.promise);
    h.receive(search("old")); h.receive(search("new"));
    newer.resolve(response("new.rs"));
    await vi.waitFor(() => expect(h.postMessage).toHaveBeenCalledTimes(1));
    if (oldFails) { older.reject(new Error("old failure")); } else { older.resolve(response("old.rs")); }
    await Promise.resolve(); await Promise.resolve();
    expect(h.postMessage).toHaveBeenCalledTimes(1);
    expect(h.postMessage.mock.calls[0][0].matches[0].path).toBe("new.rs");
    h.provider.dispose();
  });
  it("opens relative results using the daemon's resolved repository root", async () => {
    const h = setup(); h.client.contentSearch.mockResolvedValue(response("src/a.rs"));
    h.receive(search("needle"));
    await vi.waitFor(() => expect(h.postMessage).toHaveBeenCalledTimes(1));
    const open = vi.spyOn(vscode.workspace, "openTextDocument");
    h.receive({ command: "openFile", path: "src/a.rs", line: 1 });
    await vi.waitFor(() => expect(open).toHaveBeenCalled());
    expect((open.mock.calls[0][0] as any).fsPath).toBe("/repo/src/a.rs");
    h.provider.dispose();
  });
  it("does not publish pending responses after disposal", async () => {
    const h = setup(); const pending = deferred(); h.client.contentSearch.mockReturnValue(pending.promise);
    h.receive(search("needle")); h.provider.dispose(); pending.resolve(response("a.rs"));
    await Promise.resolve(); await Promise.resolve();
    expect(h.postMessage).not.toHaveBeenCalled();
  });
});
