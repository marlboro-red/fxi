import { describe, it, expect } from "vitest";
import { getWebviewContent } from "./getWebviewContent";

// Mock webview object (the function only uses it for type, doesn't call methods)
const mockWebview = {} as any;

describe("getWebviewContent", () => {
  const nonce = "abc123test";
  const defaultLimit = 200;
  const defaultContextLines = 2;

  function getHtml() {
    return getWebviewContent(mockWebview, nonce, defaultLimit, defaultContextLines);
  }

  it("returns valid HTML document", () => {
    const html = getHtml();
    expect(html).toContain("<!DOCTYPE html>");
    expect(html).toContain("<html lang=\"en\">");
    expect(html).toContain("</html>");
  });

  it("includes Content-Security-Policy with nonce", () => {
    const html = getHtml();
    expect(html).toContain(`nonce-${nonce}`);
    expect(html).toContain("Content-Security-Policy");
    // CSP should restrict to nonce-based scripts and styles
    expect(html).toContain(`style-src 'nonce-${nonce}'`);
    expect(html).toContain(`script-src 'nonce-${nonce}'`);
  });

  it("applies nonce to style and script tags", () => {
    const html = getHtml();
    expect(html).toContain(`<style nonce="${nonce}">`);
    expect(html).toContain(`<script nonce="${nonce}">`);
  });

  it("includes default limit value in input", () => {
    const html = getHtml();
    expect(html).toContain(`value="${defaultLimit}"`);
  });

  it("includes default context lines value in input", () => {
    const html = getHtml();
    expect(html).toContain(`value="${defaultContextLines}"`);
  });

  it("uses different defaults when provided", () => {
    const html = getWebviewContent(mockWebview, "x", 500, 5);
    expect(html).toContain('value="500"');
    expect(html).toContain('value="5"');
  });

  it("includes search input", () => {
    const html = getHtml();
    expect(html).toContain('id="queryInput"');
    expect(html).toContain('placeholder="Search..."');
  });

  it("includes search button", () => {
    const html = getHtml();
    expect(html).toContain('id="searchBtn"');
    expect(html).toContain("Search</button>");
  });

  it("includes files-only checkbox", () => {
    const html = getHtml();
    expect(html).toContain('id="filesOnly"');
    expect(html).toContain("Files only");
  });

  it("includes daemon banner", () => {
    const html = getHtml();
    expect(html).toContain('id="banner"');
    expect(html).toContain("Daemon not running");
    expect(html).toContain('id="bannerStartBtn"');
  });

  it("includes build index button", () => {
    const html = getHtml();
    expect(html).toContain('id="buildIndexBtn"');
    expect(html).toContain("Build Index");
  });

  it("includes results container", () => {
    const html = getHtml();
    expect(html).toContain('id="results"');
  });

  it("includes loading indicator", () => {
    const html = getHtml();
    expect(html).toContain('id="loading"');
    expect(html).toContain("Searching...");
  });

  it("sends ready message on script load", () => {
    const html = getHtml();
    expect(html).toContain("vscode.postMessage({ command: 'ready' })");
  });

  it("escapes HTML in escapeHtml function", () => {
    const html = getHtml();
    // Verify the escapeHtml function exists and handles &, <, >, "
    expect(html).toContain("function escapeHtml(s)");
    expect(html).toContain("&amp;");
    expect(html).toContain("&lt;");
    expect(html).toContain("&gt;");
    expect(html).toContain("&quot;");
  });

  it("uses Number.isFinite for numeric input parsing with clamping", () => {
    const html = getHtml();
    // Verify the num() helper uses proper numeric check (not falsy check)
    expect(html).toContain("Number.isFinite(v)");
    // Verify clamping with Math.max/Math.min
    expect(html).toContain("Math.max(min");
    expect(html).toContain("Math.min(max");
  });

  it("restores filesOnly state with explicit undefined check", () => {
    const html = getHtml();
    // Verify the fix: should use !== undefined, not truthiness check
    expect(html).toContain("prevState.filesOnly !== undefined");
  });

  it("restores saved state on load", () => {
    const html = getHtml();
    expect(html).toContain("vscode.getState()");
    expect(html).toContain("prevState.query");
    expect(html).toContain("prevState.limit");
    expect(html).toContain("prevState.contextLines");
  });

  it("saves state with vscode.setState", () => {
    const html = getHtml();
    expect(html).toContain("vscode.setState(");
  });

  it("handles connection state changes", () => {
    const html = getHtml();
    expect(html).toContain("function updateConnectionUI");
    expect(html).toContain("case 'connection':");
  });

  it("handles search results rendering", () => {
    const html = getHtml();
    expect(html).toContain("function renderResults");
    expect(html).toContain("case 'searchResults':");
  });

  it("handles error rendering with action buttons", () => {
    const html = getHtml();
    expect(html).toContain("function renderError");
    expect(html).toContain("case 'error':");
  });

  it("supports match highlighting", () => {
    const html = getHtml();
    expect(html).toContain("function highlightLine");
    expect(html).toContain("match-highlight");
  });

  it("uses VS Code CSS variables for theming", () => {
    const html = getHtml();
    expect(html).toContain("--vscode-foreground");
    expect(html).toContain("--vscode-input-background");
    expect(html).toContain("--vscode-button-background");
    expect(html).toContain("--vscode-sideBar-background");
  });

  it("includes keyboard handler for Enter to search", () => {
    const html = getHtml();
    expect(html).toContain("e.key === 'Enter'");
    expect(html).toContain("doSearch()");
  });

  it("debounces loading indicator", () => {
    const html = getHtml();
    expect(html).toContain("function showLoading");
    expect(html).toContain("function hideLoading");
    expect(html).toContain("setTimeout");
  });

  it("includes file open functionality", () => {
    const html = getHtml();
    expect(html).toContain("function openFile");
    expect(html).toContain("command: 'openFile'");
  });

  it("supports collapsible file groups", () => {
    const html = getHtml();
    expect(html).toContain("collapsed");
    expect(html).toContain("arrow");
  });

  it("does not include inline event handlers (CSP compliance)", () => {
    const html = getHtml();
    // Should not use onclick= or other inline handlers
    expect(html).not.toMatch(/onclick\s*=/i);
    expect(html).not.toMatch(/onsubmit\s*=/i);
    expect(html).not.toMatch(/onchange\s*=/i);
  });
});
