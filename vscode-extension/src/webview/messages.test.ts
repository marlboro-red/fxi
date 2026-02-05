import { describe, it, expect } from "vitest";
import type {
  WebviewMessage,
  HostMessage,
  SearchMessage,
  OpenFileMessage,
  ActionMessage,
  ReadyMessage,
  SearchResultsMessage,
  ErrorMessage,
  ConnectionMessage,
  FocusInputMessage,
} from "./messages";

// These tests verify the type contracts at runtime by constructing
// messages that conform to the interfaces. This catches missing fields
// or incorrect types if the protocol changes.

describe("WebviewMessage types", () => {
  it("SearchMessage has required fields", () => {
    const msg: SearchMessage = {
      command: "search",
      query: "test",
      limit: 100,
      contextLines: 2,
      filesOnly: false,
    };
    expect(msg.command).toBe("search");
    expect(msg.query).toBe("test");
    expect(msg.limit).toBe(100);
    expect(msg.contextLines).toBe(2);
    expect(msg.filesOnly).toBe(false);
  });

  it("OpenFileMessage has required fields", () => {
    const msg: OpenFileMessage = {
      command: "openFile",
      path: "src/main.rs",
      line: 42,
    };
    expect(msg.command).toBe("openFile");
    expect(msg.path).toBe("src/main.rs");
    expect(msg.line).toBe(42);
  });

  it("ActionMessage supports startDaemon and buildIndex", () => {
    const start: ActionMessage = { command: "action", action: "startDaemon" };
    const build: ActionMessage = { command: "action", action: "buildIndex" };
    expect(start.action).toBe("startDaemon");
    expect(build.action).toBe("buildIndex");
  });

  it("ReadyMessage has correct command", () => {
    const msg: ReadyMessage = { command: "ready" };
    expect(msg.command).toBe("ready");
  });

  it("WebviewMessage union covers all message types", () => {
    const messages: WebviewMessage[] = [
      { command: "search", query: "q", limit: 10, contextLines: 0, filesOnly: false },
      { command: "openFile", path: "f", line: 1 },
      { command: "action", action: "startDaemon" },
      { command: "ready" },
    ];
    expect(messages).toHaveLength(4);
  });
});

describe("HostMessage types", () => {
  it("SearchResultsMessage has required fields", () => {
    const msg: SearchResultsMessage = {
      command: "searchResults",
      matches: [
        {
          path: "src/lib.rs",
          line_number: 10,
          line_content: "fn main()",
          match_start: 3,
          match_end: 7,
          context_before: [],
          context_after: [],
        },
      ],
      duration_ms: 5.2,
      files_with_matches: 1,
    };
    expect(msg.command).toBe("searchResults");
    expect(msg.matches).toHaveLength(1);
    expect(msg.duration_ms).toBe(5.2);
  });

  it("ErrorMessage has required fields", () => {
    const msg: ErrorMessage = {
      command: "error",
      message: "Something went wrong",
    };
    expect(msg.command).toBe("error");
    expect(msg.message).toBe("Something went wrong");
  });

  it("ConnectionMessage has required fields", () => {
    const msg: ConnectionMessage = {
      command: "connection",
      connected: true,
    };
    expect(msg.command).toBe("connection");
    expect(msg.connected).toBe(true);
  });

  it("FocusInputMessage has correct command", () => {
    const msg: FocusInputMessage = { command: "focusInput" };
    expect(msg.command).toBe("focusInput");
  });

  it("HostMessage union covers all message types", () => {
    const messages: HostMessage[] = [
      { command: "searchResults", matches: [], duration_ms: 0, files_with_matches: 0 },
      { command: "error", message: "err" },
      { command: "connection", connected: false },
      { command: "focusInput" },
    ];
    expect(messages).toHaveLength(4);
  });
});
