import { describe, it, expect, vi } from "vitest";
import { EventEmitter } from "events";
import * as vscode from "vscode";
import { activate } from "./extension";
import { SearchPanelProvider } from "./webview/SearchPanelProvider";

vi.mock("./daemon/client", () => ({
  DaemonClient: class extends EventEmitter {
    connected = false;
    connect() {}
    dispose() { this.removeAllListeners(); }
  },
}));

describe("extension lifecycle", () => {
  it("disposes the search provider along with its registration", () => {
    const dispose = vi.spyOn(SearchPanelProvider.prototype, "dispose");
    const context = {
      extensionUri: vscode.Uri.file("/extension"),
      subscriptions: [],
    } as unknown as vscode.ExtensionContext;
    activate(context);
    expect(context.subscriptions.some((subscription) => subscription instanceof SearchPanelProvider)).toBe(true);
    for (const subscription of context.subscriptions) { subscription.dispose(); }
    expect(dispose).toHaveBeenCalledOnce();
    dispose.mockRestore();
  });
});
