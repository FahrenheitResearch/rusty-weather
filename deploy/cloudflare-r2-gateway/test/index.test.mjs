import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";

import { handleRequest } from "../src/index.js";

const HASH = "ab".repeat(32);
const TOKEN = "t".repeat(48);

class FakeBucket {
  constructor() {
    this.objects = new Map();
  }

  async get(key) {
    const bytes = this.objects.get(key);
    if (!bytes) return null;
    return {
      body: bytes,
      size: bytes.byteLength,
      httpEtag: `\"${key.length}-${bytes.byteLength}\"`,
    };
  }

  async put(key, value, options) {
    if (options.onlyIf) {
      assert.equal(options.onlyIf.get("If-None-Match"), "*");
      if (this.objects.has(key)) return null;
    }
    const bytes = new Uint8Array(value);
    this.objects.set(key, bytes);
    return { httpEtag: `\"${key.length}-${bytes.byteLength}\"` };
  }
}

function environment() {
  return {
    HOT_OBJECTS: new FakeBucket(),
    WRITE_BEARER_TOKEN: TOKEN,
    BUCKET_PATH: "rusty-weather-hot",
    MAX_MANIFEST_BYTES: "262144",
    MAX_OBJECT_BYTES: "67108864",
  };
}

function url(kind = "objects") {
  const suffix = kind === "manifests" ? ".json" : "";
  return `https://hot.example/rusty-weather-hot/v1/${kind}/${HASH}${suffix}`;
}

function objectUrl(bytes) {
  const hash = createHash("sha256").update(bytes).digest("hex");
  return `https://hot.example/rusty-weather-hot/v1/objects/${hash}`;
}

function v2ManifestUrl(bytes) {
  const hash = createHash("sha256").update(bytes).digest("hex");
  return `https://hot.example/rusty-weather-hot/v2/manifests/${hash}.json`;
}

function pointerUrl(requestHash = HASH) {
  return `https://hot.example/rusty-weather-hot/v2/requests/${requestHash}.json`;
}

function pointerBytes(manifestBytes, requestHash = HASH) {
  return new TextEncoder().encode(JSON.stringify({
    schema: "rw.community.hot-manifest-pointer.v1",
    request_sha256: requestHash,
    manifest_sha256: createHash("sha256").update(manifestBytes).digest("hex"),
  }));
}

function putRequest(target, bytes, token = TOKEN, extra = {}) {
  return new Request(target, {
    method: "PUT",
    headers: {
      Authorization: `Bearer ${token}`,
      "Content-Type": "application/octet-stream",
      "Content-Length": String(bytes.byteLength),
      "If-None-Match": "*",
      ...extra,
    },
    body: bytes,
  });
}

test("authenticated conditional PUT and public GET preserve exact bytes", async () => {
  const env = environment();
  const bytes = new TextEncoder().encode("signed-object");
  const target = objectUrl(bytes);
  const created = await handleRequest(putRequest(target, bytes), env);
  assert.equal(created.status, 201);
  const fetched = await handleRequest(new Request(target), env);
  assert.equal(fetched.status, 200);
  assert.equal(fetched.headers.get("Content-Type"), "application/octet-stream");
  assert.match(fetched.headers.get("Cache-Control"), /immutable/);
  assert.deepEqual(new Uint8Array(await fetched.arrayBuffer()), bytes);
});

test("existing immutable key returns 412 and never overwrites", async () => {
  const env = environment();
  const original = new TextEncoder().encode("original");
  const replacement = new TextEncoder().encode("replacement");
  const target = objectUrl(original);
  assert.equal((await handleRequest(putRequest(target, original), env)).status, 201);
  assert.equal((await handleRequest(putRequest(target, original), env)).status, 412);
  assert.equal((await handleRequest(putRequest(target, replacement), env)).status, 422);
  const hash = createHash("sha256").update(original).digest("hex");
  assert.deepEqual(env.HOT_OBJECTS.objects.get(`v1/objects/${hash}`), original);
});

test("renewable request pointer selects immutable content-addressed manifests", async () => {
  const env = environment();
  const first = new TextEncoder().encode("signed-manifest-one");
  const second = new TextEncoder().encode("signed-manifest-two");
  assert.equal((await handleRequest(putRequest(v2ManifestUrl(first), first), env)).status, 201);
  assert.equal((await handleRequest(putRequest(v2ManifestUrl(second), second), env)).status, 201);

  const pointer = pointerUrl();
  const firstPointer = putRequest(pointer, pointerBytes(first));
  firstPointer.headers.delete("If-None-Match");
  assert.equal((await handleRequest(firstPointer, env)).status, 201);
  const secondPointer = putRequest(pointer, pointerBytes(second));
  secondPointer.headers.delete("If-None-Match");
  assert.equal((await handleRequest(secondPointer, env)).status, 201);

  const fetched = await handleRequest(new Request(pointer), env);
  assert.equal(fetched.status, 200);
  assert.match(fetched.headers.get("Cache-Control"), /must-revalidate/);
  assert.deepEqual(new Uint8Array(await fetched.arrayBuffer()), pointerBytes(second));

  const replacement = putRequest(v2ManifestUrl(first), second);
  assert.equal((await handleRequest(replacement, env)).status, 422);
});

test("request pointer must be strictly shaped and bound to its path", async () => {
  const env = environment();
  for (const value of [
    { schema: "rw.community.hot-manifest-pointer.v1", request_sha256: "cd".repeat(32), manifest_sha256: HASH },
    { schema: "rw.community.hot-manifest-pointer.v2", request_sha256: HASH, manifest_sha256: HASH },
    { schema: "rw.community.hot-manifest-pointer.v1", request_sha256: HASH, manifest_sha256: HASH, extra: true },
  ]) {
    const bytes = new TextEncoder().encode(JSON.stringify(value));
    const request = putRequest(pointerUrl(), bytes);
    request.headers.delete("If-None-Match");
    assert.equal((await handleRequest(request, env)).status, 422);
  }
  assert.equal(env.HOT_OBJECTS.objects.size, 0);
});

test("object body must match its content-addressed path", async () => {
  const env = environment();
  const response = await handleRequest(
    putRequest(url(), new TextEncoder().encode("wrong hash")),
    env,
  );
  assert.equal(response.status, 422);
  assert.equal(env.HOT_OBJECTS.objects.size, 0);
});

test("wrong or missing authorization cannot write", async () => {
  const bytes = new Uint8Array([1]);
  for (const token of ["x".repeat(48), ""]) {
    const env = environment();
    const request = putRequest(url(), bytes, token);
    if (!token) request.headers.delete("Authorization");
    const response = await handleRequest(request, env);
    assert.equal(response.status, 401);
    assert.equal(env.HOT_OBJECTS.objects.size, 0);
    assert.doesNotMatch(await response.text(), new RegExp(TOKEN));
  }
});

test("closed path grammar rejects traversal, encodings, wrong bucket, and malformed hashes", async () => {
  const env = environment();
  const paths = [
    `/wrong/v1/objects/${HASH}`,
    `/rusty-weather-hot/v1/objects/${HASH}.json`,
    `/rusty-weather-hot/v1/manifests/${HASH}`,
    `/rusty-weather-hot/v1/objects/${"ab".repeat(31)}`,
    `/rusty-weather-hot/v1/objects/%2e%2e`,
    `/rusty-weather-hot/v1/objects/${HASH}/extra`,
    `/rusty-weather-hot/v2/requests/${HASH}`,
    `/rusty-weather-hot/v2/objects/${HASH}.json`,
    `/rusty-weather-hot/v2/manifests/${HASH}`,
  ];
  for (const path of paths) {
    const response = await handleRequest(new Request(`https://hot.example${path}`), env);
    assert.equal(response.status, 404, path);
  }
});

test("write boundary requires create-only octet stream and an exact bounded length", async () => {
  const bytes = new Uint8Array([1, 2, 3]);
  const cases = [
    ["If-None-Match", "missing", 428],
    ["Content-Type", "text/plain", 415],
    ["Content-Length", "999999999", 413],
  ];
  for (const [header, value, expected] of cases) {
    const env = environment();
    const request = putRequest(url("manifests"), bytes);
    if (value === "missing") request.headers.delete(header);
    else request.headers.set(header, value);
    const response = await handleRequest(request, env);
    assert.equal(response.status, expected, header);
    assert.equal(env.HOT_OBJECTS.objects.size, 0);
  }
  const mismatched = putRequest(url("manifests"), bytes);
  mismatched.headers.set("Content-Length", "2");
  assert.equal((await handleRequest(mismatched, environment())).status, 400);
});

test("GET is public but unknown keys and unsafe methods disclose nothing", async () => {
  const env = environment();
  assert.equal((await handleRequest(new Request(url()), env)).status, 404);
  const deleted = await handleRequest(new Request(url(), { method: "DELETE" }), env);
  assert.equal(deleted.status, 405);
  assert.equal(deleted.headers.get("Allow"), "GET, PUT");
});
