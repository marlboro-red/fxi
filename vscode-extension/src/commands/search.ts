import * as vscode from "vscode";
import { SearchPanelProvider } from "../webview/SearchPanelProvider";

export function registerSearchCommand(
  context: vscode.ExtensionContext,
  searchProvider: SearchPanelProvider
): void {
  context.subscriptions.push(
    vscode.commands.registerCommand("fxi.search", async () => {
      // Focus the search panel in the sidebar
      await vscode.commands.executeCommand("fxi.searchPanel.focus");
      // Focus the search input
      searchProvider.focusInput();
    })
  );
}
