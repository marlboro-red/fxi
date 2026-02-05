import * as vscode from "vscode";
import * as path from "path";
import * as crypto from "crypto";
import { DaemonClient } from "../daemon/client";
import { getWebviewContent } from "./getWebviewContent";
import { getWorkspaceRoot, getDefaultLimit, getDefaultContextLines } from "../ui/workspace";
import type { WebviewMessage, HostMessage } from "./messages";

export class SearchPanelProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = "fxi.searchPanel";

  private view?: vscode.WebviewView;
  private client: DaemonClient;

  constructor(
    private readonly extensionUri: vscode.Uri,
    client: DaemonClient
  ) {
    this.client = client;

    client.on("connectionChange", (connected: boolean) => {
      this.postMessage({ command: "connection", connected });
    });
  }

  resolveWebviewView(
    webviewView: vscode.WebviewView,
    _context: vscode.WebviewViewResolveContext,
    _token: vscode.CancellationToken
  ): void {
    this.view = webviewView;

    webviewView.webview.options = {
      enableScripts: true,
      localResourceRoots: [this.extensionUri],
    };

    const nonce = crypto.randomBytes(16).toString("hex");
    webviewView.webview.html = getWebviewContent(
      webviewView.webview,
      nonce,
      getDefaultLimit(),
      getDefaultContextLines()
    );

    webviewView.webview.onDidReceiveMessage((msg: WebviewMessage) => {
      this.handleMessage(msg);
    });

    // Re-send connection state and focus input whenever the panel becomes visible
    webviewView.onDidChangeVisibility(() => {
      if (webviewView.visible) {
        this.postMessage({ command: "connection", connected: this.client.connected });
        this.postMessage({ command: "focusInput" });
      }
    });
  }

  private postMessage(msg: HostMessage): void {
    this.view?.webview.postMessage(msg);
  }

  public focusInput(): void {
    this.postMessage({ command: "focusInput" });
  }

  private async handleMessage(msg: WebviewMessage): Promise<void> {
    switch (msg.command) {
      case "search":
        await this.handleSearch(msg);
        break;
      case "openFile":
        await this.handleOpenFile(msg);
        break;
      case "action":
        await this.handleAction(msg);
        break;
      case "ready":
        this.postMessage({ command: "connection", connected: this.client.connected });
        break;
    }
  }

  private async handleAction(msg: Extract<WebviewMessage, { command: "action" }>): Promise<void> {
    switch (msg.action) {
      case "startDaemon":
        vscode.commands.executeCommand("fxi.startDaemon");
        break;
      case "buildIndex":
        vscode.commands.executeCommand("fxi.indexWorkspace");
        break;
    }
  }

  private async handleSearch(msg: Extract<WebviewMessage, { command: "search" }>): Promise<void> {
    const root = getWorkspaceRoot();
    if (!root) {
      this.postMessage({ command: "error", message: "No workspace folder open." });
      return;
    }

    if (!this.client.connected) {
      this.postMessage({ command: "error", message: "Daemon not running." });
      return;
    }

    try {
      const resp = await this.client.contentSearch(msg.query, root, msg.limit, {
        context_before: msg.contextLines,
        context_after: msg.contextLines,
        case_insensitive: false,
        files_only: msg.filesOnly,
      });
      this.postMessage({
        command: "searchResults",
        matches: resp.matches,
        duration_ms: resp.duration_ms,
        files_with_matches: resp.files_with_matches,
      });
    } catch (e) {
      this.postMessage({
        command: "error",
        message: e instanceof Error ? e.message : String(e),
      });
    }
  }

  private async handleOpenFile(msg: Extract<WebviewMessage, { command: "openFile" }>): Promise<void> {
    try {
      let filePath = msg.path;
      const root = getWorkspaceRoot();
      if (root && !path.isAbsolute(filePath)) {
        filePath = path.join(root, filePath);
      }

      const uri = vscode.Uri.file(filePath);
      const line = Math.max(0, msg.line - 1);
      const doc = await vscode.workspace.openTextDocument(uri);
      const editor = await vscode.window.showTextDocument(doc, {
        selection: new vscode.Range(line, 0, line, 0),
        preserveFocus: false,
      });
      editor.revealRange(
        new vscode.Range(line, 0, line, 0),
        vscode.TextEditorRevealType.InCenter
      );
    } catch (e) {
      vscode.window.showErrorMessage(`Failed to open file: ${e}`);
    }
  }
}
