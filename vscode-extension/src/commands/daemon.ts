import * as vscode from "vscode";
import { execFile } from "child_process";
import { DaemonClient } from "../daemon/client";
import { getBinaryPath } from "../ui/workspace";

export function registerDaemonCommands(
  context: vscode.ExtensionContext,
  client: DaemonClient
): void {
  context.subscriptions.push(
    vscode.commands.registerCommand("fxi.startDaemon", async () => {
      const bin = getBinaryPath();
      try {
        await new Promise<void>((resolve, reject) => {
          execFile(bin, ["daemon", "start", "--watch"], { timeout: 15000 }, (error) => {
            if (error) { reject(error); } else { resolve(); }
          });
        });
      } catch (error) {
        vscode.window.showErrorMessage(`Failed to start FXI daemon: ${error}`);
        return;
      }

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

        let msg =
          `FXI Daemon — Uptime: ${status.uptime_secs}s | ` +
            `Indexes: ${status.indexes_loaded} | ` +
            `Docs: ${status.total_docs} | ` +
            `Queries: ${status.queries_served} | ` +
            `Cache: ${cacheRate}% | ` +
            `Memory: ${memMB} MB | ` +
            `Roots: ${roots}`;
        if (status.server_version) {
          msg += ` | Version: ${status.server_version}`;
        }
        if (status.protocol_version) {
          msg += ` | Protocol: v${status.protocol_version}`;
        }
        vscode.window.showInformationMessage(msg);
      } catch (e) {
        vscode.window.showErrorMessage(`Failed to get daemon status: ${e}`);
      }
    })
  );
}
