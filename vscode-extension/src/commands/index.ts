import * as vscode from "vscode";
import { DaemonClient } from "../daemon/client";
import { getBinaryPath, getWorkspaceRoot } from "../ui/workspace";

export function registerIndexCommands(
  context: vscode.ExtensionContext,
  client: DaemonClient
): void {
  context.subscriptions.push(
    vscode.commands.registerCommand("fxi.indexWorkspace", async () => {
      const root = getWorkspaceRoot();
      if (!root) {
        vscode.window.showWarningMessage("No workspace folder open.");
        return;
      }

      const bin = getBinaryPath();
      const task = new vscode.Task(
        { type: "fxi", task: "index" },
        vscode.TaskScope.Workspace,
        "Build Index",
        "fxi",
        new vscode.ShellExecution(bin, ["index", "--force", root])
      );

      // After the task completes, reload the index in the daemon
      const disposable = vscode.tasks.onDidEndTaskProcess((e) => {
        if (e.execution.task === task && e.exitCode === 0 && client.connected) {
          client.reload(root).catch(() => {});
        }
        disposable.dispose();
      });
      context.subscriptions.push(disposable);

      await vscode.tasks.executeTask(task);
    })
  );
}
