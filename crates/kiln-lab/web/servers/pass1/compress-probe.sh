#!/bin/bash
# Do h2o and Caddy compress a proxied event stream, and with which settings? On localhost only.
set -u
G=$HOME/.cache/kiln-bp/g1; D=$G/compress-probe; rm -rf $D; mkdir -p $D
cat > $D/up.mjs <<'JS'
import { createServer } from "node:http";
const [sock] = process.argv.slice(2);
createServer((req, res) => {
  res.writeHead(200, { "content-type": req.url.includes("fetch") ? "application/x-ndjson" : "text/event-stream", "cache-control": "no-store" });
  res.flushHeaders();
  let i = 0;
  const t = setInterval(() => { res.write(`data: {"type":"headroom","runner":"1c6ad59249a46c6c166d1432117875c2","i":${i++}}\n\n`); if (i > 50) { clearInterval(t); res.end(); } }, 10);
}).listen(sock);
JS
node $D/up.mjs $D/up.sock & UP=$!
sleep 1
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout $D/k.pem -out $D/c.pem -days 1 -subj /CN=localhost 2>/dev/null
for v in ctype ctypegz; do
  extra=""; [ $v != default ] && extra='file.mime.addtypes:
  "text/event-stream": {extensions: [".sse"], is_compressible: YES}
  "application/x-ndjson": {extensions: [".ndjson"], is_compressible: YES}'
  cat > $D/h2o-$v.conf <<C
listen:
  port: 18443
  host: 127.0.0.1
  ssl:
    certificate-file: $D/c.pem
    key-file: $D/k.pem
num-threads: 1
$extra
hosts:
  "127.0.0.1:18443":
    paths:
      /:
        proxy.reverse.url: "http://[unix:$D/up.sock]/"
        compress: [ $([ $v = ctypegz ] && echo gzip || echo br, gzip) ]
C
  $G/tools/h2o/build/h2o -c $D/h2o-$v.conf > $D/h2o-$v.log 2>&1 & H=$!
  sleep 1.5
  for p in /s /fetch; do echo "h2o $v $p: $(curl -sk --http2 -N -H 'accept-encoding: gzip, br' -D - -o $D/body https://127.0.0.1:18443$p | grep -i -E '^content-encoding|^content-type' | tr -d '\r' | tr '\n' ' ') bytes $(wc -c < $D/body)"; done
  kill $H; wait $H 2>/dev/null
done
for v in min0; do
  ml=""; [ $v = min0 ] && ml="minimum_length 1"
  cat > $D/Caddyfile-$v <<C
{
	admin off
	auto_https off
}
https://localhost:18444 {
	tls $D/c.pem $D/k.pem
	encode {
		zstd
		gzip
		$ml
		match {
			header Content-Type text/*
			header Content-Type application/json*
			header Content-Type application/x-ndjson*
		}
	}
	reverse_proxy unix/$D/up.sock {
		flush_interval -1
	}
}
C
  $G/tools/caddy/caddy run --config $D/Caddyfile-$v --adapter caddyfile > $D/caddy-$v.log 2>&1 & C=$!
  sleep 4
  curl -sk --http2 -o /dev/null -w 'caddy probe %{http_code}\n' https://localhost:18444/s 2>&1 | tail -2
  for p in /s /fetch; do echo "caddy $v $p: $(curl -sSk --http2 -N -H 'accept-encoding: gzip, zstd' -D - -o $D/body https://localhost:18444$p | grep -i -E '^content-encoding|^content-type' | tr -d '\r' | tr '\n' ' ') bytes $(wc -c < $D/body)"; done
  kill $C; wait $C 2>/dev/null
done
kill $UP
tail -3 $D/h2o-ctype.log
