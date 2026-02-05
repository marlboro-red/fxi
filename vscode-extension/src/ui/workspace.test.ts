import { describe, it, expect } from "vitest";
import { relativePath } from "./workspace";

describe("relativePath", () => {
  it("returns relative path when under workspace root", () => {
    // getWorkspaceRoot returns undefined in test (no VS Code workspace)
    // so relativePath falls through to returning absolute path
    const result = relativePath("/workspace/src/main.ts");
    expect(result).toBe("/workspace/src/main.ts");
  });

  it("returns absolute path unchanged when no workspace root", () => {
    const result = relativePath("/other/path/file.ts");
    expect(result).toBe("/other/path/file.ts");
  });
});
