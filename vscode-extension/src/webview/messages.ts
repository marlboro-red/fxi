// Messages between the webview and the extension host

import type { ContentMatch } from "../daemon/protocol";

// --- Webview → Extension Host ---

export interface SearchMessage {
  command: "search";
  query: string;
  limit: number;
  contextLines: number;
  filesOnly: boolean;
}

export interface OpenFileMessage {
  command: "openFile";
  path: string;
  line: number;
}

export interface ActionMessage {
  command: "action";
  action: "startDaemon" | "buildIndex";
}

export interface ReadyMessage {
  command: "ready";
}

export type WebviewMessage = SearchMessage | OpenFileMessage | ActionMessage | ReadyMessage;

// --- Extension Host → Webview ---

export interface SearchResultsMessage {
  command: "searchResults";
  matches: ContentMatch[];
  duration_ms: number;
  files_with_matches: number;
}

export interface ErrorMessage {
  command: "error";
  message: string;
}

export interface ConnectionMessage {
  command: "connection";
  connected: boolean;
}

export interface FocusInputMessage {
  command: "focusInput";
}

export type HostMessage =
  | SearchResultsMessage
  | ErrorMessage
  | ConnectionMessage
  | FocusInputMessage;
