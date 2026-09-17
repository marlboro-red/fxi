#!/usr/bin/env python3
"""Isolated protocol correctness probe; not a latency benchmark."""
import hashlib, json, os, pathlib, socket, struct, subprocess, tempfile, time
binary = pathlib.Path('target/release/fxi').resolve()
with tempfile.TemporaryDirectory(prefix='fxi-audit-idle-') as directory:
    base = pathlib.Path(directory)
    # Short socket name avoids macOS sockaddr_un path length.
    socket_path = f'/tmp/fxi-idle-{os.getpid()}.sock'
    env = {**os.environ, 'FXI_SOCKET': socket_path, 'FXI_INDEXES': str(base/'indexes'), 'XDG_RUNTIME_DIR': str(base)}
    log = open(base/'daemon.log', 'w+')
    daemon = subprocess.Popen([str(binary), 'daemon', 'foreground'], env=env, stdout=log, stderr=log)
    clients = []
    try:
        for _ in range(100):
            if pathlib.Path(socket_path).exists(): break
            time.sleep(.05)
        if not pathlib.Path(socket_path).exists():
            log.flush(); log.seek(0)
            raise RuntimeError(log.read())
        for _ in range(64):
            client = socket.socket(socket.AF_UNIX)
            client.connect(socket_path)
            clients.append(client)
        time.sleep(32)
        clients[0].settimeout(2)
        header = clients[0].recv(4)
        size = struct.unpack('<I', header)[0] if len(header) == 4 else 0
        response = clients[0].recv(size).decode() if size else None
        extra = socket.socket(socket.AF_UNIX)
        extra.settimeout(2)
        extra.connect(socket_path)
        try:
            payload = json.dumps({'type':'Ping', 'request_id':'audit'}).encode()
            extra.sendall(struct.pack('<I',len(payload))+payload)
            extra_result = repr(extra.recv(4096))
        except OSError as error:
            extra_result = f'{type(error).__name__}: {error}'
        extra.close()
        log.flush(); log.seek(0)
        print(json.dumps({'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(), 'idle_timeout_response':response, 'new_client_after_32_seconds':extra_result, 'log':log.read()}, indent=2))
    finally:
        for client in clients: client.close()
        daemon.terminate()
        try: daemon.wait(timeout=5)
        except subprocess.TimeoutExpired: daemon.kill(); daemon.wait()
        pathlib.Path(socket_path).unlink(missing_ok=True)
        log.close()
