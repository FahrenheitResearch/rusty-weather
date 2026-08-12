import http from "node:http";

const token = "distributed-lab-cloudflare-api-token-0001";
const username = "distributed-lab-turn-user";
const credential = "distributed-lab-turn-password-0001";
const key = "0123456789abcdef0123456789abcdef";

function drain(req, maximum = 65536) {
  return new Promise((resolve, reject) => {
    let size = 0;
    const chunks = [];
    req.on("data", (chunk) => {
      size += chunk.length;
      if (size > maximum) {
        reject(new Error("body too large"));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on("end", () => resolve(Buffer.concat(chunks)));
    req.on("error", reject);
  });
}

const server = http.createServer(async (req, res) => {
  res.setHeader("cache-control", "no-store");
  if (req.method !== "POST" || req.headers.authorization !== `Bearer ${token}`) {
    res.writeHead(404).end();
    return;
  }
  const create = `/v1/turn/keys/${key}/credentials/generate`;
  const revokePrefix = `/v1/turn/keys/${key}/credentials/`;
  if (req.url === create) {
    try {
      const body = JSON.parse((await drain(req)).toString("utf8"));
      if (!Number.isInteger(body.ttl) || body.ttl < 1 || body.ttl > 900 ||
          typeof body.customIdentifier !== "string" || body.customIdentifier.length > 128) {
        res.writeHead(400).end();
        return;
      }
    } catch {
      res.writeHead(400).end();
      return;
    }
    const response = JSON.stringify({
      iceServers: {
        urls: ["turn:turn.cloudflare.com:3478?transport=udp"],
        username,
        credential,
      },
    });
    res.writeHead(201, {
      "content-type": "application/json",
      "content-length": Buffer.byteLength(response),
    }).end(response);
    return;
  }
  if (req.url?.startsWith(revokePrefix) && req.url.endsWith("/revoke")) {
    await drain(req).catch(() => undefined);
    res.writeHead(204).end();
    return;
  }
  res.writeHead(404).end();
});

server.listen(8080, "0.0.0.0");
