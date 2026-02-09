// Protocol types mirroring src/server/protocol.rs
// Wire format: 4 bytes LE u32 length + UTF-8 JSON payload
// Response uses #[serde(tag = "type")] with newtype variants,
// so inner struct fields are flattened alongside "type".

/// Protocol version number. Bumped only on breaking changes.
export const PROTOCOL_VERSION = 2;

// --- Request types ---

export interface SearchRequest {
  type: "Search";
  query: string;
  root_path?: string;
  limit: number;
  request_id?: string;
}

export interface ContentSearchRequest {
  type: "ContentSearch";
  pattern: string;
  root_path?: string;
  limit: number;
  options: ContentSearchOptions;
  request_id?: string;
}

export interface ContentSearchOptions {
  context_before: number;
  context_after: number;
  case_insensitive: boolean;
  files_only: boolean;
}

export interface StatusRequest {
  type: "Status";
  request_id?: string;
}

export interface ReloadRequest {
  type: "Reload";
  root_path?: string;
  request_id?: string;
}

export interface ShutdownRequest {
  type: "Shutdown";
  request_id?: string;
}

export interface PingRequest {
  type: "Ping";
  request_id?: string;
}

export interface HelloRequest {
  type: "Hello";
  protocol_version: number;
  request_id?: string;
}

export type Request =
  | SearchRequest
  | ContentSearchRequest
  | StatusRequest
  | ReloadRequest
  | ShutdownRequest
  | PingRequest
  | HelloRequest;

// --- Response types ---
// Serde #[serde(tag = "type")] with newtype variants flattens inner fields.

export interface SearchMatchData {
  path: string;
  line_number: number;
  score: number;
}

export interface SearchResponse {
  type: "Search";
  matches: SearchMatchData[];
  duration_ms: number;
  cached: boolean;
  resolved_root?: string;
  request_id?: string;
}

export interface ContentMatch {
  path: string;
  line_number: number;
  line_content: string;
  match_start: number;
  match_end: number;
  context_before: [number, string][];
  context_after: [number, string][];
}

export interface ContentSearchResponse {
  type: "ContentSearch";
  matches: ContentMatch[];
  duration_ms: number;
  files_with_matches: number;
  resolved_root?: string;
  request_id?: string;
}

export interface StatusResponse {
  type: "Status";
  uptime_secs: number;
  indexes_loaded: number;
  total_docs: number;
  queries_served: number;
  cache_hit_rate: number;
  memory_bytes: number;
  loaded_roots: string[];
  protocol_version?: number;
  server_version?: string;
  request_id?: string;
}

export interface ReloadedResponse {
  type: "Reloaded";
  success: boolean;
  message: string;
  resolved_root?: string;
  request_id?: string;
}

export interface ShuttingDownResponse {
  type: "ShuttingDown";
  request_id?: string;
}

export interface PongResponse {
  type: "Pong";
  request_id?: string;
}

export interface ErrorResponse {
  type: "Error";
  message: string;
  request_id?: string;
}

export interface HelloResponse {
  type: "Hello";
  protocol_version: number;
  server_version: string;
  request_id?: string;
}

export type Response =
  | SearchResponse
  | ContentSearchResponse
  | StatusResponse
  | ReloadedResponse
  | ShuttingDownResponse
  | PongResponse
  | ErrorResponse
  | HelloResponse;
