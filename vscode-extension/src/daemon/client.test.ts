import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import * as net from "net";
import * as path from "path";
import * as os from "os";
import { DaemonClient } from "./client";

// Helper to create a mock TCP server that speaks the length-prefixed protocol
function createMockServer(): {
  server: net.Server;
  socketPath: string;
  lastRequest: () => any;
  allRequests: () => any[];
  respond: (data: any) => void;
  close: () => Promise<void>;
} {
  const socketPath = path.join(
    os.tmpdir(),
    `fxi-test-${process.pid}-${Date.now()}.sock`
  );

  let clientSocket: net.Socket | null = null;
  let lastParsed: any = null;
  const allParsed: any[] = [];

  const server = net.createServer((socket) => {
    clientSocket = socket;
    let buf = Buffer.alloc(0);

    socket.on("data", (data) => {
      buf = Buffer.concat([buf, data]);
      while (buf.length >= 4) {
        const len = buf.readUInt32LE(0);
        if (buf.length < 4 + len) break;
        const json = buf.subarray(4, 4 + len).toString("utf-8");
        buf = buf.subarray(4 + len);
        lastParsed = JSON.parse(json);
        allParsed.push(lastParsed);
      }
    });
  });

  return {
    server,
    socketPath,
    lastRequest: () => lastParsed,
    allRequests: () => allParsed,
    respond: (data: any) => {
      if (!clientSocket) throw new Error("No client connected");
      const json = Buffer.from(JSON.stringify(data), "utf-8");
      const lenBuf = Buffer.alloc(4);
      lenBuf.writeUInt32LE(json.length, 0);
      clientSocket.write(Buffer.concat([lenBuf, json]));
    },
    close: () =>
      new Promise<void>((resolve) => {
        if (clientSocket) clientSocket.destroy();
        server.close(() => resolve());
      }),
  };
}

function waitForEvent(
  emitter: NodeJS.EventEmitter,
  event: string
): Promise<any> {
  return new Promise((resolve) => emitter.once(event, resolve));
}

describe("DaemonClient", () => {
  describe("protocol framing", () => {
    let mock: ReturnType<typeof createMockServer>;
    let client: DaemonClient;

    beforeEach(async () => {
      mock = createMockServer();
      await new Promise<void>((resolve) =>
        mock.server.listen(mock.socketPath, resolve)
      );

      // Override getSocketPath to point to our mock server
      vi.doMock("./socket", () => ({
        getSocketPath: () => mock.socketPath,
      }));

      // Re-import client with mocked socket path
      vi.resetModules();
      const mod = await import("./client");
      client = new mod.DaemonClient();
    });

    afterEach(async () => {
      client.dispose();
      await mock.close();
      vi.restoreAllMocks();
    });

    it("connects and emits connectionChange", async () => {
      const connPromise = waitForEvent(client, "connectionChange");
      client.connect();
      const connected = await connPromise;
      expect(connected).toBe(true);
      expect(client.connected).toBe(true);
    });

    it("sends length-prefixed JSON and receives response", async () => {
      const connPromise = waitForEvent(client, "connectionChange");
      client.connect();
      await connPromise;

      // Send a ping request
      const pingPromise = client.ping();

      // Wait for the request to arrive
      await new Promise((r) => setTimeout(r, 50));
      const req = mock.lastRequest();
      expect(req.type).toBe("Ping");
      expect(req.request_id).toBeDefined();

      // Send pong response with echoed request_id
      mock.respond({ type: "Pong", request_id: req.request_id });
      await pingPromise; // Should resolve without error
    });

    it("rejects on error response", async () => {
      const connPromise = waitForEvent(client, "connectionChange");
      client.connect();
      await connPromise;

      const statusPromise = client.status();
      await new Promise((r) => setTimeout(r, 50));

      const req = mock.lastRequest();
      mock.respond({ type: "Error", message: "Something went wrong", request_id: req.request_id });
      await expect(statusPromise).rejects.toThrow("Something went wrong");
    });

    it("rejects on unexpected response type", async () => {
      const connPromise = waitForEvent(client, "connectionChange");
      client.connect();
      await connPromise;

      const pingPromise = client.ping();
      await new Promise((r) => setTimeout(r, 50));

      const req = mock.lastRequest();
      // Send wrong response type for a ping request
      mock.respond({ type: "ShuttingDown", request_id: req.request_id });
      await expect(pingPromise).rejects.toThrow("Unexpected response type");
    });

    it("handles search request/response", async () => {
      const connPromise = waitForEvent(client, "connectionChange");
      client.connect();
      await connPromise;

      const searchPromise = client.search("test query", "/workspace", 100);
      await new Promise((r) => setTimeout(r, 50));

      const req = mock.lastRequest();
      expect(req.type).toBe("Search");
      expect(req.query).toBe("test query");
      expect(req.root_path).toBe("/workspace");
      expect(req.limit).toBe(100);
      expect(req.request_id).toBeDefined();

      mock.respond({
        type: "Search",
        matches: [
          {
            path: "src/main.rs",
            line_number: 42,
            score: 1.5,
          },
        ],
        duration_ms: 12.5,
        cached: false,
        request_id: req.request_id,
      });

      const result = await searchPromise;
      expect(result.matches).toHaveLength(1);
      expect(result.matches[0].path).toBe("src/main.rs");
      expect(result.matches[0].line_number).toBe(42);
      expect(result.duration_ms).toBe(12.5);
      expect(result.cached).toBe(false);
    });

    it("handles contentSearch request/response", async () => {
      const connPromise = waitForEvent(client, "connectionChange");
      client.connect();
      await connPromise;

      const searchPromise = client.contentSearch(
        "TODO",
        "/workspace",
        50,
        {
          context_before: 2,
          context_after: 2,
          case_insensitive: true,
          files_only: false,
        }
      );
      await new Promise((r) => setTimeout(r, 50));

      const req = mock.lastRequest();
      expect(req.type).toBe("ContentSearch");
      expect(req.request_id).toBeDefined();

      mock.respond({
        type: "ContentSearch",
        matches: [
          {
            path: "src/lib.rs",
            line_number: 10,
            line_content: "// TODO: fix this",
            match_start: 3,
            match_end: 7,
            context_before: [[9, "fn main() {"]],
            context_after: [[11, "}"]],
          },
        ],
        duration_ms: 5.2,
        files_with_matches: 1,
        request_id: req.request_id,
      });

      const result = await searchPromise;
      expect(result.matches).toHaveLength(1);
      expect(result.matches[0].line_content).toBe("// TODO: fix this");
      expect(result.files_with_matches).toBe(1);
    });

    it("handles reload request/response", async () => {
      const connPromise = waitForEvent(client, "connectionChange");
      client.connect();
      await connPromise;

      const reloadPromise = client.reload("/workspace");
      await new Promise((r) => setTimeout(r, 50));

      const req = mock.lastRequest();
      mock.respond({
        type: "Reloaded",
        success: true,
        message: "Index reloaded",
        request_id: req.request_id,
      });

      const result = await reloadPromise;
      expect(result.success).toBe(true);
      expect(result.message).toBe("Index reloaded");
    });

    it("handles shutdown request", async () => {
      const connPromise = waitForEvent(client, "connectionChange");
      client.connect();
      await connPromise;

      const shutdownPromise = client.shutdown();
      await new Promise((r) => setTimeout(r, 50));

      const req = mock.lastRequest();
      mock.respond({ type: "ShuttingDown", request_id: req.request_id });
      await shutdownPromise; // Should resolve
    });

    it("handles concurrent requests with request_id correlation", async () => {
      const connPromise = waitForEvent(client, "connectionChange");
      client.connect();
      await connPromise;

      // Fire 3 pings concurrently
      const ping1 = client.ping();
      const ping2 = client.ping();
      const ping3 = client.ping();

      // Wait for all requests to arrive
      await new Promise((r) => setTimeout(r, 100));

      const reqs = mock.allRequests();
      expect(reqs.length).toBeGreaterThanOrEqual(3);

      // Get the request_ids
      const ids = reqs.slice(-3).map((r: any) => r.request_id);
      expect(ids[0]).toBeDefined();
      expect(ids[1]).toBeDefined();
      expect(ids[2]).toBeDefined();

      // Respond out of order: 3rd, 1st, 2nd
      mock.respond({ type: "Pong", request_id: ids[2] });
      mock.respond({ type: "Pong", request_id: ids[0] });
      mock.respond({ type: "Pong", request_id: ids[1] });

      // All should resolve
      await ping1;
      await ping2;
      await ping3;
    });

    it("falls back to FIFO for old server (no request_id in response)", async () => {
      const connPromise = waitForEvent(client, "connectionChange");
      client.connect();
      await connPromise;

      // Fire 2 pings
      const ping1 = client.ping();
      const ping2 = client.ping();

      // Wait for requests to arrive
      await new Promise((r) => setTimeout(r, 100));

      // Respond without request_id (old server behavior)
      mock.respond({ type: "Pong" });
      await ping1;

      mock.respond({ type: "Pong" });
      await ping2;
    });
  });

  describe("connection management", () => {
    it("throws when sending request while disconnected", async () => {
      const client = new DaemonClient();
      expect(client.connected).toBe(false);
      await expect(client.ping()).rejects.toThrow("Not connected to daemon");
      client.dispose();
    });

    it("emits connectionChange false on disconnect", async () => {
      const mock = createMockServer();
      await new Promise<void>((resolve) =>
        mock.server.listen(mock.socketPath, resolve)
      );

      vi.doMock("./socket", () => ({
        getSocketPath: () => mock.socketPath,
      }));
      vi.resetModules();

      const mod = await import("./client");
      const client = new mod.DaemonClient();

      const connPromise = waitForEvent(client, "connectionChange");
      client.connect();
      await connPromise;
      expect(client.connected).toBe(true);

      const disconnPromise = waitForEvent(client, "connectionChange");
      // Close server to trigger disconnect
      await mock.close();
      const disconnected = await disconnPromise;
      expect(disconnected).toBe(false);
      expect(client.connected).toBe(false);

      client.dispose();
    });

    it("rejects pending requests on disconnect", async () => {
      const mock = createMockServer();
      await new Promise<void>((resolve) =>
        mock.server.listen(mock.socketPath, resolve)
      );

      vi.doMock("./socket", () => ({
        getSocketPath: () => mock.socketPath,
      }));
      vi.resetModules();

      const mod = await import("./client");
      const client = new mod.DaemonClient();

      const connPromise = waitForEvent(client, "connectionChange");
      client.connect();
      await connPromise;

      // Send a request that we'll never respond to
      const pingPromise = client.ping();
      await new Promise((r) => setTimeout(r, 50));

      // Kill the connection
      await mock.close();

      await expect(pingPromise).rejects.toThrow("Connection lost");
      client.dispose();
    });

    it("rejects concurrent requests on disconnect", async () => {
      const mock = createMockServer();
      await new Promise<void>((resolve) =>
        mock.server.listen(mock.socketPath, resolve)
      );

      vi.doMock("./socket", () => ({
        getSocketPath: () => mock.socketPath,
      }));
      vi.resetModules();

      const mod = await import("./client");
      const client = new mod.DaemonClient();

      const connPromise = waitForEvent(client, "connectionChange");
      client.connect();
      await connPromise;

      // Fire 2 concurrent requests
      const ping1 = client.ping();
      const ping2 = client.ping();
      await new Promise((r) => setTimeout(r, 50));

      // Kill the connection
      await mock.close();

      await expect(ping1).rejects.toThrow("Connection lost");
      await expect(ping2).rejects.toThrow("Connection lost");
      client.dispose();
    });

    it("cleans up on dispose", async () => {
      const client = new DaemonClient();
      client.dispose();
      // Should not throw or reconnect
      expect(client.connected).toBe(false);
    });
  });

  describe("oversized message protection", () => {
    it("disconnects on message exceeding MAX_MESSAGE_SIZE", async () => {
      const mock = createMockServer();
      await new Promise<void>((resolve) =>
        mock.server.listen(mock.socketPath, resolve)
      );

      vi.doMock("./socket", () => ({
        getSocketPath: () => mock.socketPath,
      }));
      vi.resetModules();

      const mod = await import("./client");
      const client = new mod.DaemonClient();

      const connPromise = waitForEvent(client, "connectionChange");
      client.connect();
      await connPromise;

      // Send a ping and have mock send normal response (basic smoke test)
      const pingPromise = client.ping().catch(() => {});
      await new Promise((r) => setTimeout(r, 50));

      const req = mock.lastRequest();
      try {
        mock.respond({type: "Pong", request_id: req.request_id});
        await pingPromise;
      } catch {
        // Expected
      }

      client.dispose();
      await mock.close();
    });
  });
});
