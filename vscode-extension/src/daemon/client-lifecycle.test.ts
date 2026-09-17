import { describe, it, expect, vi, afterEach } from 'vitest';
import { EventEmitter } from 'events';

const sockets: Array<EventEmitter & {setTimeout: ReturnType<typeof vi.fn>, destroy: ReturnType<typeof vi.fn>}> = [];
vi.mock('net', () => ({createConnection: () => {
  const socket = Object.assign(new EventEmitter(), {setTimeout: vi.fn(), destroy: vi.fn()});
  sockets.push(socket);
  return socket;
}}));
vi.mock('./socket', () => ({getSocketPath: () => '/unused/audit.sock'}));
import { DaemonClient } from './client';

describe('pending socket ownership', () => {
  afterEach(() => { sockets.length = 0; vi.useRealTimers(); });
  it('coalesces simultaneous connection attempts', () => {
    const client = new DaemonClient();
    client.connect(); client.connect();
    expect(sockets.length).toBe(1);
    client.dispose();
  });
  it('ignores events from a superseded connection', () => {
    vi.useFakeTimers();
    const client = new DaemonClient();
    client.connect();
    const first = sockets[0];
    const staleClose = first.listeners('close')[0];
    first.emit('error', new Error('failed'));
    vi.advanceTimersByTime(3000);
    const second = sockets[1];
    second.emit('connect');
    staleClose();
    expect(client.connected).toBe(true);
    expect(second.destroy).not.toHaveBeenCalled();
    client.dispose();
  });
  it('disposes pending sockets without resurrection', () => {
    const client = new DaemonClient();
    client.connect(); const socket = sockets[0];
    client.dispose();
    expect(socket.destroy).toHaveBeenCalledOnce();
    socket.emit('connect');
    expect(() => socket.emit('error', new Error('late connection error'))).not.toThrow();
    expect(client.connected).toBe(false);
    client.dispose();
  });
});
