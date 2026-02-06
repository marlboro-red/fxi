import * as vscode from "vscode";
import { DaemonClient } from "../daemon/client";
import { getBinaryPath } from "../ui/workspace";

function shellQuote(s: string): string {
  if (process.platform === "win32") {
    // PowerShell / cmd: wrap in double quotes, escape inner double quotes
    return `"${s.replace(/"/g, '""')}"`;
  }
  // Unix: wrap in single quotes, escape inner single quotes
  return `'${s.replace(/'/g, "'\\''")}'`;
}

export function registerDaemonCommands(
  context: vscode.ExtensionContext,
  client: DaemonClient
): void {
  context.subscriptions.push(
    vscode.commands.registerCommand("fxi.startDaemon", async () => {
      const bin = getBinaryPath();
      const terminal = vscode.window.createTerminal({ name: "FXI Daemon", hideFromUser: true });
      terminal.sendText(`${shellQuote(bin)} daemon start --watch`);

      // Try to connect with retries to verify daemon actually started
      let connected = false;
      for (let i = 0; i < 5; i++) {
        await new Promise((r) => setTimeout(r, 800));
        client.connect();
        // Give connection attempt time to resolve
        await new Promise((r) => setTimeout(r, 400));
        if (client.connected) {
          connected = true;
          break;
        }
      }

      if (connected) {
        vscode.window.showInformationMessage("FXI daemon started.");
      } else {
        const action = await vscode.window.showWarningMessage(
          "FXI daemon did not respond. Is fxi installed and on your PATH?",
          "Retry",
          "Open Settings"
        );
        if (action === "Retry") {
          vscode.commands.executeCommand("fxi.startDaemon");
        } else if (action === "Open Settings") {
          vscode.commands.executeCommand("workbench.action.openSettings", "fxi.binaryPath");
        }
      }
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
