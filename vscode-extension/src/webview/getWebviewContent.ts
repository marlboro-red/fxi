import * as vscode from "vscode";

export function getWebviewContent(
  _webview: vscode.Webview,
  nonce: string,
  defaultLimit: number,
  defaultContextLines: number
): string {
  return /* html */ `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'nonce-${nonce}'; script-src 'nonce-${nonce}';">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <style nonce="${nonce}">
    * { box-sizing: border-box; margin: 0; padding: 0; }

    body {
      font-family: var(--vscode-font-family);
      font-size: var(--vscode-font-size);
      color: var(--vscode-foreground);
      background: var(--vscode-sideBar-background);
      padding: 8px;
    }

    /* --- Banner --- */
    .banner {
      display: flex;
      align-items: center;
      gap: 8px;
      padding: 8px 10px;
      margin-bottom: 8px;
      border-radius: 4px;
      font-size: 12px;
    }
    .banner.hidden { display: none; }
    .banner-disconnected {
      background: var(--vscode-inputValidation-warningBackground, rgba(200, 140, 0, 0.15));
      border: 1px solid var(--vscode-inputValidation-warningBorder, rgba(200, 140, 0, 0.4));
    }
    .banner-text { flex: 1; }
    .banner-btn {
      padding: 3px 10px;
      border: none;
      border-radius: 3px;
      cursor: pointer;
      font-family: inherit;
      font-size: 11px;
      white-space: nowrap;
      background: var(--vscode-button-background);
      color: var(--vscode-button-foreground);
    }
    .banner-btn:hover {
      background: var(--vscode-button-hoverBackground);
    }

    /* --- Search box --- */
    .search-box {
      display: flex;
      margin-bottom: 6px;
    }
    .search-box input {
      flex: 1;
      padding: 4px 8px;
      background: var(--vscode-input-background);
      color: var(--vscode-input-foreground);
      border: 1px solid var(--vscode-input-border, transparent);
      border-radius: 2px;
      outline: none;
      font-family: inherit;
      font-size: inherit;
    }
    .search-box input:focus {
      border-color: var(--vscode-focusBorder);
    }
    .search-box input::placeholder {
      color: var(--vscode-input-placeholderForeground);
    }
    .search-box input:disabled {
      opacity: 0.5;
    }

    /* --- Options row --- */
    .options {
      display: flex;
      gap: 8px;
      align-items: center;
      margin-bottom: 8px;
      flex-wrap: wrap;
      font-size: 11px;
    }
    .options label {
      display: flex;
      align-items: center;
      gap: 3px;
      cursor: pointer;
    }
    .options input[type="checkbox"] {
      accent-color: var(--vscode-focusBorder);
    }
    .field {
      display: flex;
      align-items: center;
      gap: 3px;
      font-size: 11px;
    }
    .field-narrow input[type="number"] {
      width: 44px;
    }
    .field-wide input[type="number"] {
      width: 72px;
    }
    .field input[type="number"] {
      padding: 1px 4px;
      background: var(--vscode-input-background);
      color: var(--vscode-input-foreground);
      border: 1px solid var(--vscode-input-border, transparent);
      border-radius: 2px;
      font-family: inherit;
      font-size: 11px;
    }

    /* --- Subtle action link --- */
    .subtle-actions {
      margin-bottom: 8px;
      font-size: 11px;
    }
    .subtle-link {
      color: var(--vscode-textLink-foreground);
      cursor: pointer;
      background: none;
      border: none;
      font-family: inherit;
      font-size: 11px;
      padding: 0;
      text-decoration: none;
    }
    .subtle-link:hover {
      text-decoration: underline;
    }

    /* --- Stats --- */
    .stats-bar {
      padding: 4px 0;
      font-size: 11px;
      color: var(--vscode-descriptionForeground);
      border-bottom: 1px solid var(--vscode-panel-border, var(--vscode-sideBar-border, transparent));
      margin-bottom: 4px;
    }
    .stats-bar.hidden { display: none; }

    /* --- Status / empty / error --- */
    .status-msg {
      padding: 16px 8px;
      text-align: center;
      color: var(--vscode-descriptionForeground);
      font-size: 12px;
      line-height: 1.6;
    }
    .status-msg.hidden { display: none; }
    .status-msg .inline-action {
      display: inline-block;
      margin-top: 8px;
      padding: 4px 12px;
      border: none;
      border-radius: 3px;
      cursor: pointer;
      font-family: inherit;
      font-size: 12px;
      background: var(--vscode-button-background);
      color: var(--vscode-button-foreground);
    }
    .status-msg .inline-action:hover {
      background: var(--vscode-button-hoverBackground);
    }
    .status-msg .hint {
      display: block;
      margin-top: 4px;
      font-size: 11px;
      color: var(--vscode-descriptionForeground);
    }

    /* --- Results --- */
    .results { overflow-y: auto; }

    .file-group { margin-bottom: 2px; }
    .file-header {
      padding: 2px 4px;
      font-weight: bold;
      font-size: 12px;
      color: var(--vscode-textLink-foreground);
      cursor: pointer;
      display: flex;
      align-items: center;
      gap: 4px;
      border-radius: 2px;
    }
    .file-header:hover { background: var(--vscode-list-hoverBackground); }
    .file-header .arrow { font-size: 10px; width: 12px; }
    .file-header .match-count {
      color: var(--vscode-descriptionForeground);
      font-weight: normal;
    }

    .file-lines { padding-left: 4px; }
    .file-lines.collapsed { display: none; }

    .match-group {
      padding: 1px 0;
    }
    .match-separator {
      height: 1px;
      margin: 3px 0 3px 36px;
      background: var(--vscode-panel-border, var(--vscode-sideBar-border, rgba(128,128,128,0.2)));
    }

    .content-line {
      display: flex;
      font-family: var(--vscode-editor-font-family, monospace);
      font-size: 12px;
      cursor: pointer;
      border-radius: 2px;
      line-height: 1.4;
    }
    .content-line:hover { background: var(--vscode-list-hoverBackground); }
    .line-number {
      min-width: 36px;
      text-align: right;
      padding-right: 8px;
      color: var(--vscode-editorLineNumber-foreground);
      user-select: none;
    }
    .line-text {
      flex: 1;
      white-space: pre;
      overflow: hidden;
      text-overflow: ellipsis;
    }
    .context-line .line-text {
      color: var(--vscode-descriptionForeground);
    }
    .match-highlight {
      background: var(--vscode-editor-findMatchHighlightBackground, rgba(234, 92, 0, 0.33));
      border-radius: 1px;
    }

    .loading {
      display: flex;
      align-items: center;
      justify-content: center;
      padding: 20px;
      color: var(--vscode-descriptionForeground);
    }
    .loading.hidden { display: none; }
  </style>
</head>
<body>

  <div class="banner banner-disconnected" id="banner">
    <span class="banner-text">Daemon not running</span>
    <button class="banner-btn" id="bannerStartBtn">Start Daemon</button>
  </div>

  <div class="search-box">
    <input type="text" id="queryInput" placeholder="Search..." autofocus />
  </div>

  <div class="options">
    <label><input type="checkbox" id="filesOnly" /> Files only</label>
    <div class="field field-narrow">
      <label for="contextInput">Context:</label>
      <input type="number" id="contextInput" value="${defaultContextLines}" min="0" max="20" />
    </div>
    <div class="field field-wide">
      <label for="limitInput">Limit:</label>
      <input type="number" id="limitInput" value="${defaultLimit}" min="1" max="10000" />
    </div>
  </div>

  <div class="subtle-actions">
    <button class="subtle-link" id="buildIndexBtn" title="Build the search index for this workspace (fxi index --force)">Build Index</button>
  </div>

  <div class="stats-bar hidden" id="statsBar"></div>
  <div class="loading hidden" id="loading">Searching...</div>
  <div class="status-msg" id="statusMsg"></div>
  <div class="results" id="results"></div>

  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();

    const banner = document.getElementById('banner');
    const bannerStartBtn = document.getElementById('bannerStartBtn');
    const queryInput = document.getElementById('queryInput');
    const limitInput = document.getElementById('limitInput');
    const contextInput = document.getElementById('contextInput');
    const filesOnly = document.getElementById('filesOnly');
    const buildIndexBtn = document.getElementById('buildIndexBtn');
    const statsBar = document.getElementById('statsBar');
    const loading = document.getElementById('loading');
    const statusMsg = document.getElementById('statusMsg');
    const results = document.getElementById('results');

    let connected = false;
    let hasSearched = false;

    // Restore state
    const prevState = vscode.getState();
    if (prevState) {
      if (prevState.query) queryInput.value = prevState.query;
      if (prevState.limit != null) limitInput.value = prevState.limit;
      if (prevState.contextLines != null) contextInput.value = prevState.contextLines;
      if (prevState.filesOnly !== undefined) filesOnly.checked = prevState.filesOnly;
    }

    function saveState() {
      vscode.setState({
        query: queryInput.value,
        limit: limitInput.value,
        contextLines: contextInput.value,
        filesOnly: filesOnly.checked,
      });
    }

    function showStatus(html) {
      statusMsg.innerHTML = html;
      statusMsg.classList.remove('hidden');
    }

    function hideStatus() {
      statusMsg.classList.add('hidden');
    }

    function updateConnectionUI(isConnected) {
      connected = isConnected;
      queryInput.disabled = !isConnected;

      if (isConnected) {
        banner.classList.add('hidden');
        if (!hasSearched) {
          queryInput.placeholder = 'Search...';
          showStatus('Type a query and press Enter to search.'
            + '<span class="hint">Supports full fxi syntax: "exact phrase", ext:rs, re:/pattern/, near:a,b,5</span>');
        }
      } else {
        banner.classList.remove('hidden');
        queryInput.placeholder = 'Daemon not running...';
        if (!hasSearched) {
          showStatus('Start the daemon to begin searching.'
            + '<br><button class="inline-action" id="statusStartDaemon">Start Daemon</button>');
          const btn = document.getElementById('statusStartDaemon');
          if (btn) btn.addEventListener('click', () => {
            vscode.postMessage({ command: 'action', action: 'startDaemon' });
          });
        }
      }
    }

    // --- Actions ---
    bannerStartBtn.addEventListener('click', () => {
      vscode.postMessage({ command: 'action', action: 'startDaemon' });
    });
    buildIndexBtn.addEventListener('click', () => {
      vscode.postMessage({ command: 'action', action: 'buildIndex' });
    });

    // --- Search ---
    let lastFilesOnly = false;

    function num(el, fallback) {
      const v = parseInt(el.value);
      return Number.isFinite(v) ? v : fallback;
    }

    function doSearch() {
      const query = queryInput.value.trim();
      if (!query) return;

      hasSearched = true;
      saveState();
      results.innerHTML = '';
      statsBar.classList.add('hidden');
      hideStatus();
      loading.classList.remove('hidden');

      lastFilesOnly = filesOnly.checked;

      vscode.postMessage({
        command: 'search',
        query,
        limit: num(limitInput, ${defaultLimit}),
        contextLines: num(contextInput, ${defaultContextLines}),
        filesOnly: filesOnly.checked,
      });
    }

    queryInput.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') doSearch();
    });

    // --- Rendering helpers ---
    function escapeHtml(s) {
      return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
    }

    function openFile(path, line) {
      vscode.postMessage({ command: 'openFile', path, line });
    }

    function highlightLine(lineContent, matchStart, matchEnd) {
      if (matchStart >= matchEnd || matchStart >= lineContent.length) {
        return escapeHtml(lineContent);
      }
      const before = escapeHtml(lineContent.slice(0, matchStart));
      const match = escapeHtml(lineContent.slice(matchStart, matchEnd));
      const after = escapeHtml(lineContent.slice(matchEnd));
      return before + '<span class="match-highlight">' + match + '</span>' + after;
    }

    function renderMatchGroup(m) {
      let html = '<div class="match-group">';

      if (m.context_before) {
        for (const [ln, text] of m.context_before) {
          html += '<div class="content-line context-line" data-path="' + escapeHtml(m.path) + '" data-line="' + ln + '">'
            + '<span class="line-number">' + ln + '</span>'
            + '<span class="line-text">' + escapeHtml(text) + '</span></div>';
        }
      }

      html += '<div class="content-line match-line" data-path="' + escapeHtml(m.path) + '" data-line="' + m.line_number + '">'
        + '<span class="line-number">' + m.line_number + '</span>'
        + '<span class="line-text">' + highlightLine(m.line_content, m.match_start, m.match_end) + '</span></div>';

      if (m.context_after) {
        for (const [ln, text] of m.context_after) {
          html += '<div class="content-line context-line" data-path="' + escapeHtml(m.path) + '" data-line="' + ln + '">'
            + '<span class="line-number">' + ln + '</span>'
            + '<span class="line-text">' + escapeHtml(text) + '</span></div>';
        }
      }

      html += '</div>';
      return html;
    }

    function renderResults(data) {
      loading.classList.add('hidden');
      const matches = data.matches;
      const isFilesOnly = lastFilesOnly;

      if (isFilesOnly) {
        statsBar.textContent = data.files_with_matches + ' files \\u00b7 ' + data.duration_ms.toFixed(1) + 'ms';
      } else {
        statsBar.textContent = matches.length + ' matches \\u00b7 ' + data.files_with_matches + ' files \\u00b7 ' + data.duration_ms.toFixed(1) + 'ms';
      }
      statsBar.classList.remove('hidden');

      if (matches.length === 0) {
        showStatus('No results found.');
        return;
      }

      if (isFilesOnly) {
        const paths = [];
        for (const m of matches) {
          if (paths.length === 0 || paths[paths.length - 1] !== m.path) {
            paths.push(m.path);
          }
        }

        let html = '';
        for (const p of paths) {
          html += '<div class="file-header" data-path="' + escapeHtml(p) + '" data-line="1">'
            + escapeHtml(p) + '</div>';
        }
        results.innerHTML = html;

        results.querySelectorAll('.file-header').forEach(el => {
          el.addEventListener('click', () => {
            openFile(el.dataset.path, parseInt(el.dataset.line));
          });
        });
        return;
      }

      // Group matches by file
      const groups = new Map();
      for (const m of matches) {
        if (!groups.has(m.path)) groups.set(m.path, []);
        groups.get(m.path).push(m);
      }

      let html = '';
      let groupIdx = 0;
      for (const [filePath, fileMatches] of groups) {
        const gid = 'g' + groupIdx++;
        html += '<div class="file-group">';
        html += '<div class="file-header" data-gid="' + gid + '">'
          + '<span class="arrow">\\u25be</span>'
          + escapeHtml(filePath)
          + ' <span class="match-count">(' + fileMatches.length + ')</span>'
          + '</div>';
        html += '<div class="file-lines" id="' + gid + '">';

        for (let i = 0; i < fileMatches.length; i++) {
          if (i > 0) {
            html += '<div class="match-separator"></div>';
          }
          html += renderMatchGroup(fileMatches[i]);
        }

        html += '</div></div>';
      }
      results.innerHTML = html;

      results.querySelectorAll('.file-header').forEach(el => {
        el.addEventListener('click', () => {
          const gid = el.dataset.gid;
          const lines = document.getElementById(gid);
          const arrow = el.querySelector('.arrow');
          if (lines.classList.contains('collapsed')) {
            lines.classList.remove('collapsed');
            arrow.textContent = '\\u25be';
          } else {
            lines.classList.add('collapsed');
            arrow.textContent = '\\u25b8';
          }
        });
      });

      results.querySelectorAll('.content-line').forEach(el => {
        el.addEventListener('click', () => {
          openFile(el.dataset.path, parseInt(el.dataset.line));
        });
      });
    }

    function renderError(msg) {
      loading.classList.add('hidden');
      statsBar.classList.add('hidden');

      let extra = '';
      const lower = msg.toLowerCase();
      if (lower.includes('daemon') || lower.includes('not connected') || lower.includes('not running')) {
        extra = '<br><button class="inline-action" id="errStartDaemon">Start Daemon</button>';
      } else if (lower.includes('index') || lower.includes('no such file') || lower.includes('not found')) {
        extra = '<br><button class="inline-action" id="errBuildIndex">Build Index</button>';
      }

      showStatus(escapeHtml(msg) + extra);

      const startBtn = document.getElementById('errStartDaemon');
      if (startBtn) startBtn.addEventListener('click', () => {
        vscode.postMessage({ command: 'action', action: 'startDaemon' });
      });
      const indexBtn = document.getElementById('errBuildIndex');
      if (indexBtn) indexBtn.addEventListener('click', () => {
        vscode.postMessage({ command: 'action', action: 'buildIndex' });
      });
    }

    // --- Message handling ---
    window.addEventListener('message', (event) => {
      const msg = event.data;
      switch (msg.command) {
        case 'searchResults':
          renderResults(msg);
          break;
        case 'error':
          renderError(msg.message);
          break;
        case 'connection':
          updateConnectionUI(msg.connected);
          break;
        case 'focusInput':
          queryInput.focus();
          break;
      }
    });

    // Tell the host we're ready — it will reply with connection state
    vscode.postMessage({ command: 'ready' });
  </script>
</body>
</html>`;
}
