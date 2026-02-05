// Protocol types mirroring src/server/protocol.rs
// Wire format: 4 bytes LE u32 length + UTF-8 JSON payload
// Response uses #[serde(tag = "type")] with newtype variants,
// so inner struct fields are flattened alongside "type".

// --- Request types ---

export interface SearchRequest {
  type: "Search";
  query: string;
  root_path: string;
  limit: number;
}

export interface ContentSearchRequest {
  type: "ContentSearch";
  pattern: string;
  root_path: string;
  limit: number;
  options: ContentSearchOptions;
}

export interface ContentSearchOptions {
  context_before: number;
  context_after: number;
  case_insensitive: boolean;
  files_only: boolean;
}

export interface StatusRequest {
  type: "Status";
}

export interface ReloadRequest {
  type: "Reload";
  root_path: string;
}

export interface ShutdownRequest {
  type: "Shutdown";
}

export interface PingRequest {
  type: "Ping";
}

export type Request =
  | SearchRequest
  | ContentSearchRequest
  | StatusRequest
  | ReloadRequest
  | ShutdownRequest
  | PingRequest;

// --- Response types ---
// Serde #[serde(tag = "type")] with newtype variants flattens inner fields.

export interface SearchMatchData {
  doc_id: number;
  path: string;
  line_number: number;
  score: number;
}

export interface SearchResponse {
  type: "Search";
  matches: SearchMatchData[];
  duration_ms: number;
  cached: boolean;
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
}

export interface ReloadedResponse {
  type: "Reloaded";
  success: boolean;
  message: string;
}

export interface ShuttingDownResponse {
  type: "ShuttingDown";
}

export interface PongResponse {
  type: "Pong";
}

export interface ErrorResponse {
  type: "Error";
  message: string;
}

export type Response =
  | SearchResponse
  | ContentSearchResponse
  | StatusResponse
  | ReloadedResponse
  | ShuttingDownResponse
  | PongResponse
  | ErrorResponse;
