// Executed by test-dev-image.py with `node --input-type=module -e` inside ahvm.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdir, readFile, writeFile } from "node:fs/promises";

const [phase, expected] = process.argv.slice(1);
assert.ok(["init", "reopen"].includes(phase));
assert.equal(process.cwd(), "/workspace");
assert.equal(process.env.HOME, "/home/ahvm");
assert.notEqual(process.getuid(), 0, "gate must run as ahvm");
const capability = JSON.parse(execFileSync("ahvm-pi-durable-check", { encoding: "utf8", timeout: 15000 }));
assert.equal(capability.available, true);
for (const version of Object.values(capability.packages)) assert.equal(version, expected);

const moduleRoot = "file:///opt/ahvm-pi-durable/node_modules";
const { BACKGROUND_CONTEXT: context } = await import(`${moduleRoot}/@earendil-works/chord/dist/context/index.js`);
const { createModels } = await import(`${moduleRoot}/@earendil-works/pi-ai/dist/models.js`);
const { fauxProvider, fauxAssistantMessage } = await import(`${moduleRoot}/@earendil-works/pi-ai/dist/providers/faux.js`);
const { openaiCodexProvider } = await import(`${moduleRoot}/@earendil-works/pi-ai/dist/providers/openai-codex.js`);
const { openaiCodexOAuth } = await import(`${moduleRoot}/@earendil-works/pi-ai/dist/auth/oauth/openai-codex.js`);
const { Harness, AssistantEntry, createRegistry } = await import(`${moduleRoot}/@earendil-works/pi-durable/dist/index.js`);
const { openNodeSqliteStorage } = await import(`${moduleRoot}/@earendil-works/pi-durable/dist/storage/sqlite/node.js`);
const models = createModels();
const faux = fauxProvider();
models.setProvider(faux.provider);
models.setProvider(openaiCodexProvider());
assert.equal(models.getModel("openai-codex", "gpt-6.1-sol").api, "openai-codex-responses");
assert.equal(typeof openaiCodexOAuth.login, "function");
assert.equal(typeof openaiCodexOAuth.refresh, "function");
// Exercise headless device admission and cancellation with a fake HTTP response;
// no real device code, account credential, or authorization request is created.
const originalFetch = globalThis.fetch;
const cancelled = new AbortController();
let oauthRequests = 0;
let deviceEvents = 0;
globalThis.fetch = async (url) => {
  assert.equal(String(url), "https://auth.openai.com/api/accounts/deviceauth/usercode");
  oauthRequests++;
  return new Response(JSON.stringify({ device_auth_id: "image-gate-fake-device", user_code: "FAKE-CODE", interval: "1" }),
    { status: 200, headers: { "Content-Type": "application/json" } });
};
try {
  await assert.rejects(openaiCodexOAuth.login({ signal: cancelled.signal,
    prompt: async prompt => { assert.equal(prompt.type, "select"); return "device_code"; },
    notify: event => {
      assert.equal(event.type, "device_code");
      assert.equal(event.verificationUri, "https://auth.openai.com/codex/device");
      deviceEvents++;
      cancelled.abort();
    },
  }), /cancelled|aborted|abort/i);
  assert.equal(deviceEvents, 1);
  assert.equal(oauthRequests, 1, "cancel must stop device polling before a second request");
} finally {
  globalThis.fetch = originalFetch;
}
if (phase === "init") faux.setResponses([fauxAssistantMessage("PI-DURABLE-FAUX-OK")]);
const directory = "/workspace/.ahvm-pi-image-gate";
if (phase === "init") await mkdir(directory, { mode: 0o700 });
const harness = await Harness.open(await openNodeSqliteStorage(`${directory}/session.sqlite`),
  { models, registry: createRegistry() }, context);
try {
  const root = await harness.root(context, { agent: {
    model: { provider: faux.getModel().provider, modelId: faux.getModel().id },
  } });
  const draft = { type: "write", requestId: "image-gate-1",
    entry: { kind: "app.image-gate", data: { marker: "PI-SQLITE-PERSISTENT" } } };
  const submission = await root.submit(draft, context);
  assert.equal((await submission.wait(context)).status, "done");
  const duplicate = await root.submit(draft, context);
  assert.equal(duplicate.id, submission.id, "duplicate admission must return same receipt");
  const input = await root.submit({ type: "input", requestId: "image-gate-faux-1", content: "Synthetic gate." }, context);
  const settled = await input.wait(context);
  assert.equal(settled.status, "done");
  const answer = await root.commit(tx => tx.entry(AssistantEntry, settled.answer), context);
  assert.equal(answer.model[0].content[0].text, "PI-DURABLE-FAUX-OK");
  assert.equal(faux.state.callCount, phase === "init" ? 1 : 0);
  const receipt = { conversation: root.id, submission: submission.id, input: input.id, answer: settled.answer };
  if (phase === "init") {
    await writeFile(`${directory}/receipt.json`, JSON.stringify(receipt), { mode: 0o600 });
  } else {
    assert.deepEqual(receipt, JSON.parse(await readFile(`${directory}/receipt.json`, "utf8")),
      "conversation and receipt must survive VM stop/start");
  }
  const entries = (await root.entries({}, 100, undefined, context)).items
    .filter(entry => entry.kind === "app.image-gate");
  assert.equal(entries.length, 1, "duplicate submission must not duplicate stored entry");
  assert.equal(entries[0].data.marker, "PI-SQLITE-PERSISTENT");
  console.log(JSON.stringify({ pi_durable_version: expected, phase, node_sqlite: true,
    duplicate_admission: true, persistent_receipt: true, faux_model_calls: faux.state.callCount,
    codex_subscription_runtime: true, mocked_device_oauth_cancel: true, external_model_calls: 0 }));
} finally {
  await harness.close(context);
}
