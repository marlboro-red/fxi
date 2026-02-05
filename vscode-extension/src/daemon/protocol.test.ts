import { describe, it, expect } from "vitest";
import type {
  Request,
  Response,
  SearchRequest,
  ContentSearchRequest,
  StatusRequest,
  ReloadRequest,
  ShutdownRequest,
  PingRequest,
  SearchResponse,
  ContentSearchResponse,
  StatusResponse,
  ReloadedResponse,
  ShuttingDownResponse,
  PongResponse,
  ErrorResponse,
  ContentSearchOptions,
  SearchMatchData,
  ContentMatch,
} from "./protocol";

// These tests verify the TypeScript protocol types match the Rust serde
// wire format. Each test constructs a JSON object as the daemon would
// serialize it and validates it conforms to our TypeScript types.

describe("Request types match Rust serde format", () => {
  it("Search request has type tag and fields", () => {
    const req: SearchRequest = {
      type: "Search",
      query: "fn main",
      root_path: "/home/user/project",
      limit: 100,
    };
    const json = JSON.parse(JSON.stringify(req));
    expect(json.type).toBe("Search");
    expect(json.query).toBe("fn main");
    expect(json.root_path).toBe("/home/user/project");
    expect(json.limit).toBe(100);
  });

  it("ContentSearch request includes options", () => {
    const options: ContentSearchOptions = {
      context_before: 2,
      context_after: 2,
      case_insensitive: false,
      files_only: true,
    };
    const req: ContentSearchRequest = {
      type: "ContentSearch",
      pattern: "TODO",
      root_path: "/workspace",
      limit: 50,
      options,
    };
    const json = JSON.parse(JSON.stringify(req));
    expect(json.type).toBe("ContentSearch");
    expect(json.options.files_only).toBe(true);
  });

  it("Status request only has type", () => {
    const req: StatusRequest = { type: "Status" };
    const json = JSON.parse(JSON.stringify(req));
    expect(Object.keys(json)).toEqual(["type"]);
  });

  it("Reload request has root_path", () => {
    const req: ReloadRequest = { type: "Reload", root_path: "/project" };
    expect(req.root_path).toBe("/project");
  });

  it("Shutdown request only has type", () => {
    const req: ShutdownRequest = { type: "Shutdown" };
    expect(Object.keys(req)).toEqual(["type"]);
  });

  it("Ping request only has type", () => {
    const req: PingRequest = { type: "Ping" };
    expect(Object.keys(req)).toEqual(["type"]);
  });

  it("Request union includes all request types", () => {
    const requests: Request[] = [
      { type: "Search", query: "q", root_path: "/r", limit: 10 },
      {
        type: "ContentSearch",
        pattern: "p",
        root_path: "/r",
        limit: 10,
        options: { context_before: 0, context_after: 0, case_insensitive: false, files_only: false },
      },
      { type: "Status" },
      { type: "Reload", root_path: "/r" },
      { type: "Shutdown" },
      { type: "Ping" },
    ];
    expect(requests).toHaveLength(6);
  });
});

describe("Response types match Rust serde format", () => {
  // Rust uses #[serde(tag = "type")] with newtype variants.
  // Newtype variant fields are flattened alongside the "type" tag.

  it("Search response has flattened fields (matches serde internally-tagged newtype)", () => {
    // Rust serializes: Response::Search(SearchResponse { matches: [...], duration_ms: 12.5, cached: false })
    // as: { "type": "Search", "matches": [...], "duration_ms": 12.5, "cached": false }
    const wireJson = {
      type: "Search" as const,
      matches: [
        { doc_id: 1, path: "src/main.rs", line_number: 42, score: 1.5 },
      ],
      duration_ms: 12.5,
      cached: false,
    };
    const resp: SearchResponse = wireJson;
    expect(resp.type).toBe("Search");
    expect(resp.matches[0].path).toBe("src/main.rs");
    expect(resp.duration_ms).toBe(12.5);
  });

  it("ContentSearch response matches wire format", () => {
    const wireJson = {
      type: "ContentSearch" as const,
      matches: [
        {
          path: "src/lib.rs",
          line_number: 10,
          line_content: "// TODO fix",
          match_start: 3,
          match_end: 7,
          context_before: [[9, "fn main() {"]] as [number, string][],
          context_after: [[11, "}"]] as [number, string][],
        },
      ],
      duration_ms: 3.1,
      files_with_matches: 1,
    };
    const resp: ContentSearchResponse = wireJson;
    expect(resp.matches[0].context_before[0][0]).toBe(9);
    expect(resp.matches[0].context_before[0][1]).toBe("fn main() {");
  });

  it("Status response matches wire format", () => {
    const wireJson = {
      type: "Status" as const,
      uptime_secs: 3600,
      indexes_loaded: 2,
      total_docs: 15000,
      queries_served: 42,
      cache_hit_rate: 0.85,
      memory_bytes: 104857600,
      loaded_roots: ["/project1", "/project2"],
    };
    const resp: StatusResponse = wireJson;
    expect(resp.uptime_secs).toBe(3600);
    expect(resp.loaded_roots).toHaveLength(2);
  });

  it("Reloaded response matches wire format", () => {
    const wireJson = {
      type: "Reloaded" as const,
      success: true,
      message: "Index reloaded for /project",
    };
    const resp: ReloadedResponse = wireJson;
    expect(resp.success).toBe(true);
  });

  it("ShuttingDown response only has type", () => {
    const wireJson = { type: "ShuttingDown" as const };
    const resp: ShuttingDownResponse = wireJson;
    expect(resp.type).toBe("ShuttingDown");
  });

  it("Pong response only has type", () => {
    const wireJson = { type: "Pong" as const };
    const resp: PongResponse = wireJson;
    expect(resp.type).toBe("Pong");
  });

  it("Error response has message", () => {
    const wireJson = {
      type: "Error" as const,
      message: "Index not found",
    };
    const resp: ErrorResponse = wireJson;
    expect(resp.message).toBe("Index not found");
  });

  it("Response union includes all response types", () => {
    const responses: Response[] = [
      { type: "Search", matches: [], duration_ms: 0, cached: false },
      { type: "ContentSearch", matches: [], duration_ms: 0, files_with_matches: 0 },
      {
        type: "Status",
        uptime_secs: 0,
        indexes_loaded: 0,
        total_docs: 0,
        queries_served: 0,
        cache_hit_rate: 0,
        memory_bytes: 0,
        loaded_roots: [],
      },
      { type: "Reloaded", success: true, message: "" },
      { type: "ShuttingDown" },
      { type: "Pong" },
      { type: "Error", message: "" },
    ];
    expect(responses).toHaveLength(7);
  });
});

describe("SearchMatchData", () => {
  it("all fields are required", () => {
    const match: SearchMatchData = {
      doc_id: 5,
      path: "src/lib.rs",
      line_number: 100,
      score: 2.3,
    };
    expect(match.doc_id).toBe(5);
    expect(match.score).toBe(2.3);
  });
});

describe("ContentMatch", () => {
  it("context arrays use tuple format [line_number, text]", () => {
    const match: ContentMatch = {
      path: "src/main.rs",
      line_number: 50,
      line_content: "let x = 42;",
      match_start: 4,
      match_end: 5,
      context_before: [
        [48, "fn main() {"],
        [49, "  // comment"],
      ],
      context_after: [
        [51, "  println!(\"{}\", x);"],
        [52, "}"],
      ],
    };
    expect(match.context_before).toHaveLength(2);
    expect(match.context_before[0][0]).toBe(48);
    expect(match.context_after[1][1]).toBe("}");
  });

  it("handles empty context arrays", () => {
    const match: ContentMatch = {
      path: "f.rs",
      line_number: 1,
      line_content: "x",
      match_start: 0,
      match_end: 1,
      context_before: [],
      context_after: [],
    };
    expect(match.context_before).toHaveLength(0);
  });
});
