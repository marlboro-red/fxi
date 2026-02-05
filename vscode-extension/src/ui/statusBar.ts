import * as vscode from "vscode";
import { DaemonClient } from "../daemon/client";

export class StatusBar implements vscode.Disposable {
  private item: vscode.StatusBarItem;
  private client: DaemonClient;
  private pollTimer: ReturnType<typeof setInterval> | null = null;

  constructor(client: DaemonClient) {
    this.client = client;
    this.item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
    this.item.command = "fxi.showStatus";
    this.update();
    this.item.show();

    client.on("connectionChange", () => this.update());

    // Poll every 10s
    this.pollTimer = setInterval(() => this.update(), 10_000);
  }

  private update(): void {
    if (this.client.connected) {
      this.item.text = "$(search) FXI";
      this.item.tooltip = "FXI daemon connected";
      this.item.backgroundColor = undefined;
    } else {
      this.item.text = "$(search) FXI $(warning)";
      this.item.tooltip = "FXI daemon disconnected";
      this.item.backgroundColor = new vscode.ThemeColor("statusBarItem.warningBackground");
    }
  }

  dispose(): void {
    if (this.pollTimer) {
      clearInterval(this.pollTimer);
    }
    this.item.dispose();
  }
}
