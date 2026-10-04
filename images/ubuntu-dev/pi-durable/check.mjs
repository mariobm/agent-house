// Read-only admission probe. Only an in-memory SQLite database is opened.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile, realpath, stat } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const root = "/opt/ahvm-pi-durable";
const manifestPath = "/usr/local/share/ahvm/pi-durable-runtime.json";

async function protectedFile(path) {
  const info = await stat(await realpath(path));
  assert.equal(info.uid, 0, `runtime asset must be root-owned: ${path}`);
  assert.equal(info.mode & 0o022, 0, `runtime asset must not be writable by guest: ${path}`);
}

try {
  await protectedFile(manifestPath);
  await protectedFile(root);
  await protectedFile(import.meta.filename);
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  assert.equal(manifest.schema, 1);
  assert.equal(manifest.harness, "pi-durable");
  assert.equal(manifest.module_root, `${root}/node_modules`);
  assert.equal(manifest.node_minimum, "22.19.0");
  assert.equal(manifest.tool_scope_helper, "/usr/local/bin/ahvm-pi-tool");
  assert.equal(manifest.tool_scope_protocol, 1);
  await protectedFile(manifest.tool_scope_helper);
  const [major, minor] = process.versions.node.split(".").map(Number);
  assert.ok(major > 22 || (major === 22 && minor >= 19), "Node 22.19+ required");
  await protectedFile(`${root}/package-lock.json`);
  const lock = await readFile(`${root}/package-lock.json`);
  assert.equal(createHash("sha256").update(lock).digest("hex"), manifest.package_lock_sha256,
    "installed dependency lock differs from image manifest");
  const packageNames = ["@earendil-works/chord", "@earendil-works/pi-ai", "@earendil-works/pi-durable"];
  assert.deepEqual(Object.keys(manifest.packages).sort(), packageNames);
  for (const name of packageNames) {
    const path = `${manifest.module_root}/${name}/package.json`;
    await protectedFile(path);
    const metadata = JSON.parse(await readFile(path, "utf8"));
    assert.equal(metadata.name, name);
    assert.equal(metadata.version, manifest.packages[name], `runtime version mismatch: ${name}`);
    assert.equal(metadata.version, "1.0.2", `unsupported runtime version: ${name}`);
  }
  for (const name of ["@earendil-works/chord/context", "@earendil-works/pi-ai/models",
    "@earendil-works/pi-ai/providers/openai-codex",
    "@earendil-works/pi-durable", "@earendil-works/pi-durable/tools",
    "@earendil-works/pi-durable/env/node", "@earendil-works/pi-durable/storage/sqlite/node"]) {
    await protectedFile(fileURLToPath(import.meta.resolve(name)));
    await import(name);
  }
  const { openNodeSqliteStorage } = await import("@earendil-works/pi-durable/storage/sqlite/node");
  const storage = await openNodeSqliteStorage(":memory:");
  await storage.close();
  console.log(JSON.stringify({ available: true, ...manifest, node_runtime: process.versions.node,
    node_sqlite: true }));
} catch (error) {
  console.error(`Pi Durable runtime unavailable: ${error.message}`);
  process.exitCode = 1;
}
