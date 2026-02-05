// Minimal vscode module mock for unit tests

export const Uri = {
  file: (path: string) => ({ fsPath: path, scheme: "file", path }),
};

export const workspace = {
  workspaceFolders: undefined as any,
  getConfiguration: (_section: string) => ({
    get: <T>(_key: string, defaultValue: T): T => defaultValue,
  }),
  openTextDocument: async (_uri: any) => ({}),
};

export const window = {
  createStatusBarItem: () => ({
    text: "",
    tooltip: "",
    command: "",
    backgroundColor: undefined,
    show: () => {},
    dispose: () => {},
  }),
  showInformationMessage: async (..._args: any[]) => undefined,
  showWarningMessage: async (..._args: any[]) => undefined,
  showErrorMessage: async (..._args: any[]) => undefined,
  showTextDocument: async (_doc: any, _opts?: any) => ({
    revealRange: () => {},
  }),
  createTerminal: (_opts: any) => ({
    sendText: (_text: string) => {},
  }),
  registerWebviewViewProvider: (_viewType: string, _provider: any) => ({
    dispose: () => {},
  }),
};

export const commands = {
  registerCommand: (_id: string, _handler: (...args: any[]) => any) => ({
    dispose: () => {},
  }),
  executeCommand: async (_id: string, ..._args: any[]) => undefined,
};

export const tasks = {
  executeTask: async (_task: any) => undefined,
  onDidEndTaskProcess: (_handler: any) => ({
    dispose: () => {},
  }),
};

export enum StatusBarAlignment {
  Left = 1,
  Right = 2,
}

export class ThemeColor {
  constructor(public id: string) {}
}

export enum TaskScope {
  Global = 1,
  Workspace = 2,
}

export class Task {
  constructor(
    public definition: any,
    public scope: any,
    public name: string,
    public source: string,
    public execution?: any
  ) {}
}

export class ShellExecution {
  constructor(public commandLine: string, public args?: string[]) {}
}

export class Range {
  constructor(
    public startLine: number,
    public startCharacter: number,
    public endLine: number,
    public endCharacter: number
  ) {}
}

export enum TextEditorRevealType {
  Default = 0,
  InCenter = 1,
  InCenterIfOutsideViewport = 2,
  AtTop = 3,
}
