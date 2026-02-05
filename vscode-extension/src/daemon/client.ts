import * as net from "net";
import { EventEmitter } from "events";
import { getSocketPath } from "./socket";
import type {
  Request,
  Response,
  SearchResponse,
  ContentSearchResponse,
  StatusResponse,
  ContentSearchOptions,
} from "./protocol";

const MAX_MESSAGE_SIZE = 100 * 1024 * 1024; // 100 MB
const RECONNECT_DELAY_MS = 3000;
const CONNECT_TIMEOUT_MS = 5000;
const REQUEST_TIMEOUT_MS = 30000;

interface PendingRequest {
  resolve: (value: Response) => void;
  reject: (reason: Error) => void;
}

export class DaemonClient extends EventEmitter {
  private socket: net.Socket | null = null;
  private buffer: Buffer = Buffer.alloc(0);
  private pending: PendingRequest | null = null;
  private queue: Array<{ request: Request; resolve: (v: Response) => void; reject: (e: Error) => void }> = [];
  private processing = false;
  private _connected = false;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private disposed = false;

  get connected(): boolean {
    return this._connected;
  }

  connect(): void {
    if (this.disposed) {return;}
    if (this.socket) {return;}

    const socketPath = getSocketPath();
    const socket = net.createConnection({ path: socketPath });
    socket.setTimeout(CONNECT_TIMEOUT_MS);

    socket.on("connect", () => {
      socket.setTimeout(0); // Clear connect timeout
      this.socket = socket;
      this._connected = true;
      this.emit("connectionChange", true);
      this.processQueue();
    });

    socket.on("data", (data: Buffer) => {
      this.onData(data);
    });

    socket.on("error", () => {
      this.handleDisconnect();
    });

    socket.on("close", () => {
      this.handleDisconnect();
    });

    socket.on("timeout", () => {
      socket.destroy();
      this.handleDisconnect();
    });
  }

  private handleDisconnect(): void {
    if (this.socket) {
      this.socket.removeAllListeners();
      this.socket.destroy();
      this.socket = null;
    }

    const wasConnected = this._connected;
    this._connected = false;
    this.buffer = Buffer.alloc(0);

    // Reject pending request
    if (this.pending) {
      this.pending.reject(new Error("Connection lost"));
      this.pending = null;
    }

    // Reject queued requests
    for (const item of this.queue) {
      item.reject(new Error("Connection lost"));
    }
    this.queue = [];
    this.processing = false;

    if (wasConnected) {
      this.emit("connectionChange", false);
    }

    this.scheduleReconnect();
  }

  private scheduleReconnect(): void {
    if (this.disposed) {return;}
    if (this.reconnectTimer) {return;}

    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, RECONNECT_DELAY_MS);
  }

  private onData(data: Buffer): void {
    this.buffer = Buffer.concat([this.buffer, data]);

    while (this.buffer.length >= 4) {
      const len = this.buffer.readUInt32LE(0);

      if (len > MAX_MESSAGE_SIZE) {
        this.handleDisconnect();
        return;
      }

      if (this.buffer.length < 4 + len) {
        break; // Wait for more data
      }

      const jsonBuf = this.buffer.subarray(4, 4 + len);
      this.buffer = this.buffer.subarray(4 + len);

      try {
        const response: Response = JSON.parse(jsonBuf.toString("utf-8"));
        if (this.pending) {
          const p = this.pending;
          this.pending = null;
          p.resolve(response);
          this.processQueue();
        }
      } catch {
        if (this.pending) {
          const p = this.pending;
          this.pending = null;
          p.reject(new Error("Invalid JSON response"));
          this.processQueue();
        }
      }
    }
  }

  private sendRaw(request: Request): Promise<Response> {
    return new Promise((resolve, reject) => {
      this.queue.push({ request, resolve, reject });
      this.processQueue();
    });
  }

  private processQueue(): void {
    if (this.processing) {return;}
    if (!this.socket || !this._connected) {return;}
    if (this.queue.length === 0) {return;}

    this.processing = true;
    const item = this.queue.shift()!;
    this.pending = { resolve: item.resolve, reject: item.reject };

    const json = JSON.stringify(item.request);
    const jsonBuf = Buffer.from(json, "utf-8");
    const lenBuf = Buffer.alloc(4);
    lenBuf.writeUInt32LE(jsonBuf.length, 0);

    try {
      this.socket.write(Buffer.concat([lenBuf, jsonBuf]), (err) => {
        if (err) {
          if (this.pending) {
            const p = this.pending;
            this.pending = null;
            p.reject(err);
          }
          this.processing = false;
          this.processQueue();
        }
      });
    } catch (e) {
      if (this.pending) {
        const p = this.pending;
        this.pending = null;
        p.reject(e instanceof Error ? e : new Error(String(e)));
      }
      this.processing = false;
    }

    // processing remains true until response arrives (handled in onData)
    // We set it false after the pending resolves
    const origResolve = item.resolve;
    const origReject = item.reject;
    if (this.pending) {
      this.pending.resolve = (v) => {
        this.processing = false;
        origResolve(v);
      };
      this.pending.reject = (e) => {
        this.processing = false;
        origReject(e);
      };
    }
  }

  private async request(req: Request): Promise<Response> {
    if (!this._connected) {
      throw new Error("Not connected to daemon");
    }
    return new Promise<Response>((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error("Request timed out"));
      }, REQUEST_TIMEOUT_MS);

      this.sendRaw(req).then(
        (v) => { clearTimeout(timer); resolve(v); },
        (e) => { clearTimeout(timer); reject(e); },
      );
    });
  }

  async search(query: string, rootPath: string, limit: number): Promise<SearchResponse> {
    const resp = await this.request({
      type: "Search",
      query,
      root_path: rootPath,
      limit,
    });
    if (resp.type === "Error") {
      throw new Error(resp.message);
    }
    if (resp.type !== "Search") {
      throw new Error(`Unexpected response type: ${resp.type}`);
    }
    return resp;
  }

  async contentSearch(
    pattern: string,
    rootPath: string,
    limit: number,
    options: ContentSearchOptions
  ): Promise<ContentSearchResponse> {
    const resp = await this.request({
      type: "ContentSearch",
      pattern,
      root_path: rootPath,
      limit,
      options,
    });
    if (resp.type === "Error") {
      throw new Error(resp.message);
    }
    if (resp.type !== "ContentSearch") {
      throw new Error(`Unexpected response type: ${resp.type}`);
    }
    return resp;
  }

  async status(): Promise<StatusResponse> {
    const resp = await this.request({ type: "Status" });
    if (resp.type === "Error") {
      throw new Error(resp.message);
    }
    if (resp.type !== "Status") {
      throw new Error(`Unexpected response type: ${resp.type}`);
    }
    return resp;
  }

  async reload(rootPath: string): Promise<{ success: boolean; message: string }> {
    const resp = await this.request({ type: "Reload", root_path: rootPath });
    if (resp.type === "Error") {
      throw new Error(resp.message);
    }
    if (resp.type !== "Reloaded") {
      throw new Error(`Unexpected response type: ${resp.type}`);
    }
    return { success: resp.success, message: resp.message };
  }

  async ping(): Promise<void> {
    const resp = await this.request({ type: "Ping" });
    if (resp.type === "Error") {
      throw new Error(resp.message);
    }
    if (resp.type !== "Pong") {
      throw new Error(`Unexpected response type: ${resp.type}`);
    }
  }

  async shutdown(): Promise<void> {
    const resp = await this.request({ type: "Shutdown" });
    if (resp.type === "Error") {
      throw new Error(resp.message);
    }
  }

  dispose(): void {
    this.disposed = true;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.socket) {
      this.socket.removeAllListeners();
      this.socket.destroy();
      this.socket = null;
    }
    this._connected = false;
  }
}
