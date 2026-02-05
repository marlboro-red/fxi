import * as vscode from "vscode";
import { DaemonClient } from "../daemon/client";
import { getBinaryPath } from "../ui/workspace";

export function registerDaemonCommands(
  context: vscode.ExtensionContext,
  client: DaemonClient
): void {
  context.subscriptions.push(
    vscode.commands.registerCommand("fxi.startDaemon", async () => {
      const bin = getBinaryPath();
      const terminal = vscode.window.createTerminal({ name: "FXI Daemon", hideFromUser: true });
      terminal.sendText(`${bin} daemon start --watch`);

      // Give daemon time to start, then reconnect
      await new Promise((r) => setTimeout(r, 1500));
      client.connect();
      vscode.window.showInformationMessage("FXI daemon started.");
    }),

    vscode.commands.registerCommand("fxi.stopDaemon", async () => {
      if (!client.connected) {
        vscode.window.showWarningMessage("FXI daemon is not connected.");
        return;
      }
      try {
        await client.shutdown();
        vscode.window.showInformationMessage("FXI daemon stopped.");
      } catch (e) {
        vscode.window.showErrorMessage(`Failed to stop daemon: ${e}`);
      }
    }),

    vscode.commands.registerCommand("fxi.showStatus", async () => {
      if (!client.connected) {
        const action = await vscode.window.showWarningMessage(
          "FXI daemon is not connected.",
          "Start Daemon"
        );
        if (action === "Start Daemon") {
          vscode.commands.executeCommand("fxi.startDaemon");
        }
        return;
      }

      try {
        const status = await client.status();
        const memMB = (status.memory_bytes / 1024 / 1024).toFixed(1);
        const cacheRate = (status.cache_hit_rate * 100).toFixed(1);
        const roots = status.loaded_roots.join(", ") || "none";

        vscode.window.showInformationMessage(
          `FXI Daemon — Uptime: ${status.uptime_secs}s | ` +
            `Indexes: ${status.indexes_loaded} | ` +
            `Docs: ${status.total_docs} | ` +
            `Queries: ${status.queries_served} | ` +
            `Cache: ${cacheRate}% | ` +
            `Memory: ${memMB} MB | ` +
            `Roots: ${roots}`
        );
      } catch (e) {
        vscode.window.showErrorMessage(`Failed to get daemon status: ${e}`);
      }
    })
  );
}
