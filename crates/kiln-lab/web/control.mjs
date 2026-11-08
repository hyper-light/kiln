// The lab's control echo (docs/browser-lab.md, "The clock"): a WebSocket over TLS on each client's
// unshaped control pair, answering each message with the server's clock in µs, so the probe page
// can find its clock's offset from the server's. It runs in the server's namespace; nothing it
// carries crosses a shaped link.
//   node control.mjs CERT KEY PORT
import { createServer } from "node:https";
import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";

const [cert, key, port] = process.argv.slice(2);
const nowUs = () => (performance.timeOrigin + performance.now()) * 1000;

function frame(text) {
  const p = Buffer.from(text);
  return Buffer.concat([Buffer.from([0x81, p.length]), p]);
}

const server = createServer({ cert: readFileSync(cert), key: readFileSync(key) }, (req, res) => {
  res.writeHead(200, { "access-control-allow-origin": "*", "cross-origin-resource-policy": "cross-origin", "content-type": "text/plain" });
  res.end(String(nowUs()));
});
server.on("upgrade", (req, /** @type {import("node:net").Socket} */ sock) => {
  const accept = createHash("sha1").update(req.headers["sec-websocket-key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest("base64");
  sock.setNoDelay(true);
  sock.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
  let buf = Buffer.alloc(0);
  sock.on("data", (/** @type {Buffer} */ d) => {
    buf = Buffer.concat([buf, d]);
    while (buf.length >= 2) {
      const op = buf[0] & 0x0f;
      let len = buf[1] & 0x7f;
      let at = 2;
      if (len === 126) {
        if (buf.length < 4) return;
        len = buf.readUInt16BE(2);
        at = 4;
      }
      if (buf.length < at + 4 + len) return;
      const mask = buf.subarray(at, at + 4);
      const body = Buffer.from(buf.subarray(at + 4, at + 4 + len));
      for (let i = 0; i < body.length; i++) body[i] ^= mask[i % 4];
      buf = buf.subarray(at + 4 + len);
      if (op === 0x8) return sock.end();
      if (op === 0x1) sock.write(frame(String(nowUs())));
    }
  });
  sock.on("error", () => {});
});
server.listen(Number(port), "0.0.0.0");
