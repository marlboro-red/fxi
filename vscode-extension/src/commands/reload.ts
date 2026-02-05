import * as vscode from "vscode";
import { DaemonClient } from "../daemon/client";
import { getWorkspaceRoot } from "../ui/workspace";

export function registerReloadCommand(
  context: vscode.ExtensionContext,
  client: DaemonClient
): void {
  context.subscriptions.push(
    vscode.commands.registerCommand("fxi.reloadIndex", async () => {
      if (!client.connected) {
        vscode.window.showWarningMessage("FXI daemon is not connected.");
        return;
      }

      const root = getWorkspaceRoot();
      if (!root) {
        vscode.window.showWarningMessage("No workspace folder open.");
        return;
      }

      try {
        const result = await client.reload(root);
        if (result.success) {
          vscode.window.showInformationMessage(`FXI: ${result.message}`);
        } else {
          vscode.window.showWarningMessage(`FXI reload failed: ${result.message}`);
        }
      } catch (e) {
        vscode.window.showErrorMessage(`Failed to reload index: ${e}`);
      }
    })
  );
}
