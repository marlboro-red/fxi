import * as vscode from "vscode";
import * as path from "path";

/**
 * Get the workspace root folder path.
 * Returns the first workspace folder, or undefined if none.
 */
export function getWorkspaceRoot(): string | undefined {
  const folders = vscode.workspace.workspaceFolders;
  if (folders && folders.length > 0) {
    return folders[0].uri.fsPath;
  }
  return undefined;
}

/**
 * Get the fxi binary path from settings.
 */
export function getBinaryPath(): string {
  return vscode.workspace.getConfiguration("fxi").get<string>("binaryPath", "fxi");
}

/**
 * Get the default result limit from settings.
 */
export function getDefaultLimit(): number {
  return vscode.workspace.getConfiguration("fxi").get<number>("defaultLimit", 200);
}

/**
 * Get the default context lines from settings.
 */
export function getDefaultContextLines(): number {
  return vscode.workspace.getConfiguration("fxi").get<number>("defaultContextLines", 2);
}

/**
 * Make a path relative to workspace root for display.
 */
export function relativePath(absolutePath: string): string {
  const root = getWorkspaceRoot();
  if (root && absolutePath.startsWith(root)) {
    return path.relative(root, absolutePath);
  }
  return absolutePath;
}
