import * as vscode from "vscode";
import { DaemonClient } from "./daemon/client";
import { StatusBar } from "./ui/statusBar";
import { SearchPanelProvider } from "./webview/SearchPanelProvider";
import { registerDaemonCommands } from "./commands/daemon";
import { registerIndexCommands } from "./commands/index";
import { registerReloadCommand } from "./commands/reload";
import { registerSearchCommand } from "./commands/search";

export function activate(context: vscode.ExtensionContext): void {
  const client = new DaemonClient();

  // Connect to daemon
  client.connect();

  // Status bar
  const statusBar = new StatusBar(client);
  context.subscriptions.push(statusBar);

  // Webview search panel
  const searchProvider = new SearchPanelProvider(context.extensionUri, client);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(
      SearchPanelProvider.viewType,
      searchProvider
    )
  );

  // Register commands
  registerDaemonCommands(context, client);
  registerIndexCommands(context, client);
  registerReloadCommand(context, client);
  registerSearchCommand(context, searchProvider);

  // Cleanup on deactivation
  context.subscriptions.push({
    dispose: () => client.dispose(),
  });
}

export function deactivate(): void {
  // Handled by dispose
}
