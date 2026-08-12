const HARD_MAX_MANIFEST_BYTES = 262_144;
const HARD_MAX_OBJECT_BYTES = 67_108_864;
const HARD_MAX_POINTER_BYTES = 1_024;
const V1_KEY_PATTERN = /^\/([a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)\/v1\/(manifests|objects)\/([a-f0-9]{64})(\.json)?$/;
const V2_KEY_PATTERN = /^\/([a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)\/v2\/(manifests|requests)\/([a-f0-9]{64})\.json$/;

function plain(status, text, headers = {}) {
  return new Response(text, {
    status,
    headers: {
      "Content-Type": "text/plain; charset=utf-8",
      "X-Content-Type-Options": "nosniff",
      ...headers,
    },
  });
}

function parsePositiveBound(value, hardMaximum) {
  if (typeof value !== "string" || !/^[1-9][0-9]{0,15}$/.test(value)) {
    return null;
  }
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) && parsed <= hardMaximum ? parsed : null;
}

function parseKey(url, env) {
  if (typeof env.BUCKET_PATH !== "string" || !/^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(env.BUCKET_PATH)) {
    return null;
  }
  const match = V1_KEY_PATTERN.exec(url.pathname) ?? V2_KEY_PATTERN.exec(url.pathname);
  if (!match || match[1] !== env.BUCKET_PATH) {
    return null;
  }
  const [, , kind, hash, suffix] = match;
  const version = url.pathname.includes("/v2/") ? "v2" : "v1";
  if (version === "v1" && (kind === "manifests") !== (suffix === ".json")) {
    return null;
  }
  const replaceable = version === "v2" && kind === "requests";
  return {
    key: `${version}/${kind}/${hash}${version === "v2" || suffix ? ".json" : ""}`,
    kind,
    hash,
    version,
    replaceable,
    maximumBytes:
      replaceable
        ? HARD_MAX_POINTER_BYTES
        : kind === "manifests"
        ? parsePositiveBound(env.MAX_MANIFEST_BYTES, HARD_MAX_MANIFEST_BYTES)
        : parsePositiveBound(env.MAX_OBJECT_BYTES, HARD_MAX_OBJECT_BYTES),
  };
}

async function sha256(bytes) {
  return new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
}

function hex(bytes) {
  return [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

async function bearerMatches(request, configuredToken) {
  if (typeof configuredToken !== "string" || configuredToken.length < 32 || configuredToken.length > 4096) {
    return false;
  }
  const header = request.headers.get("Authorization") ?? "";
  if (!header.startsWith("Bearer ") || header.length > 4103) {
    return false;
  }
  const encoder = new TextEncoder();
  const [actual, expected] = await Promise.all([
    sha256(encoder.encode(header.slice(7))),
    sha256(encoder.encode(configuredToken)),
  ]);
  let difference = actual.length ^ expected.length;
  for (let index = 0; index < Math.max(actual.length, expected.length); index += 1) {
    difference |= (actual[index] ?? 0) ^ (expected[index] ?? 0);
  }
  return difference === 0;
}

async function readBodyBounded(stream, declaredBytes, maximumBytes) {
  if (!stream) return null;
  const reader = stream.getReader();
  const body = new Uint8Array(declaredBytes);
  let total = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (!(value instanceof Uint8Array)) return null;
      total += value.byteLength;
      if (total > declaredBytes || total > maximumBytes) {
        await reader.cancel();
        return null;
      }
      body.set(value, total - value.byteLength);
    }
  } finally {
    reader.releaseLock();
  }
  if (total !== declaredBytes || total === 0) return null;
  return body;
}

function cacheRequest(request) {
  return new Request(request.url, { method: "GET" });
}

async function publicGet(request, env, parsed) {
  const cache = globalThis.caches?.default;
  const cacheKey = cacheRequest(request);
  if (cache) {
    const cached = await cache.match(cacheKey);
    if (cached) {
      return cached;
    }
  }

  const object = await env.HOT_OBJECTS.get(parsed.key);
  if (object === null) {
    return plain(404, "Not Found");
  }
  if (typeof object.size !== "number" || object.size < 0 || object.size > parsed.maximumBytes) {
    return plain(502, "Stored object violates gateway policy");
  }
  const cacheControl = parsed.replaceable
    ? "public, max-age=30, must-revalidate"
    : parsed.kind === "objects" || parsed.version === "v2"
      ? "public, max-age=31536000, immutable"
      : "public, max-age=300";
  const response = new Response(object.body, {
    status: 200,
    headers: {
      "Content-Type": "application/octet-stream",
      "Content-Length": String(object.size),
      ETag: object.httpEtag,
      "Cache-Control": cacheControl,
      "X-Content-Type-Options": "nosniff",
    },
  });
  if (cache) {
    try {
      await cache.put(cacheKey, response.clone());
    } catch {
      // R2 remains authoritative for this hot tier; edge cache failure is a
      // cost/performance event, never a reason to hide a verified object.
    }
  }
  return response;
}

async function immutablePut(request, env, parsed) {
  if (!(await bearerMatches(request, env.WRITE_BEARER_TOKEN))) {
    return plain(401, "Unauthorized", { "WWW-Authenticate": "Bearer" });
  }
  if (!parsed.replaceable && request.headers.get("If-None-Match") !== "*") {
    return plain(428, "If-None-Match: * is required");
  }
  if (parsed.replaceable && request.headers.has("If-None-Match")) {
    return plain(400, "Replaceable request pointers must not use If-None-Match");
  }
  const contentType = (request.headers.get("Content-Type") ?? "").split(";", 1)[0].trim().toLowerCase();
  if (contentType !== "application/octet-stream") {
    return plain(415, "Unsupported Media Type");
  }
  const contentLength = request.headers.get("Content-Length");
  if (contentLength === null || !/^[1-9][0-9]{0,15}$/.test(contentLength)) {
    return plain(411, "A nonzero Content-Length is required");
  }
  const declared = Number(contentLength);
  if (!Number.isSafeInteger(declared) || declared > parsed.maximumBytes) {
    return plain(413, "Payload Too Large");
  }
  const body = await readBodyBounded(request.body, declared, parsed.maximumBytes);
  if (body === null) {
    return plain(400, "Body length mismatch");
  }
  if ((parsed.kind === "objects" || (parsed.version === "v2" && parsed.kind === "manifests"))
      && hex(await sha256(body)) !== parsed.hash) {
    return plain(422, "Body SHA-256 does not match its content-addressed key");
  }
  if (parsed.replaceable && !validPointerBody(body, parsed.hash)) {
    return plain(422, "Malformed or mismatched request pointer");
  }
  const options = { httpMetadata: { contentType: "application/octet-stream" } };
  if (!parsed.replaceable) options.onlyIf = request.headers;
  const stored = await env.HOT_OBJECTS.put(parsed.key, body, options);
  if (stored === null) {
    return plain(412, "Precondition Failed");
  }
  if (parsed.replaceable && globalThis.caches?.default) {
    try {
      await globalThis.caches.default.delete(cacheRequest(request));
    } catch {
      // Pointer freshness is also bounded by its short cache lifetime and every
      // referenced manifest is independently hashed and signature-verified.
    }
  }
  return new Response(null, {
    status: 201,
    headers: {
      ETag: stored.httpEtag,
      "Cache-Control": "no-store",
      "X-Content-Type-Options": "nosniff",
    },
  });
}

function validPointerBody(body, requestHash) {
  let value;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(body));
  } catch {
    return false;
  }
  if (!value || Array.isArray(value) || typeof value !== "object") return false;
  const keys = Object.keys(value).sort();
  if (keys.join(",") !== "manifest_sha256,request_sha256,schema") return false;
  return value.schema === "rw.community.hot-manifest-pointer.v1"
    && value.request_sha256 === requestHash
    && /^[a-f0-9]{64}$/.test(value.manifest_sha256);
}

export async function handleRequest(request, env) {
  if (!env?.HOT_OBJECTS || typeof env.HOT_OBJECTS.get !== "function" || typeof env.HOT_OBJECTS.put !== "function") {
    return plain(503, "Gateway unavailable");
  }
  let url;
  try {
    url = new URL(request.url);
  } catch {
    return plain(400, "Bad Request");
  }
  if (url.protocol !== "https:" || url.username || url.password || url.search || url.hash) {
    return plain(400, "Bad Request");
  }
  const parsed = parseKey(url, env);
  if (!parsed || parsed.maximumBytes === null) {
    return plain(404, "Not Found");
  }
  try {
    if (request.method === "GET") {
      return await publicGet(request, env, parsed);
    }
    if (request.method === "PUT") {
      return await immutablePut(request, env, parsed);
    }
    return plain(405, "Method Not Allowed", { Allow: "GET, PUT" });
  } catch {
    return plain(502, "Gateway operation failed");
  }
}

export default {
  fetch: handleRequest,
};
