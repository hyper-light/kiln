# The probe's own command (plan step 26, step 4): run by sh as harness antigravity, with its
# harness.json's env, in the microVM. It writes what it finds into /workspace/probe, the run's
# project volume, and ends with /workspace/probe/done. Every result is a file; nothing stops it.
o=/workspace/probe
mkdir -p "$o" || exit 1
agy --version > "$o/version" 2> "$o/version.err"; echo $? > "$o/version.rc"
id -u > "$o/uid"; id -g > "$o/gid"
printf '%s\n' "$HOME" > "$o/home"
getent hosts cloudcode-pa.googleapis.com > "$o/dns" 2>&1; echo $? > "$o/dns.rc"
touch "$HOME/x" 2> "$o/touch.err"; echo $? > "$o/touch.rc"
git config --global user.name probe 2> "$o/git.err"; echo $? > "$o/git.rc"
agy mcp add kiln-probe /bin/true > "$o/mcp.out" 2>&1; echo $? > "$o/mcp.rc"
printf '%s\n' '{"event":"user","message":{"content":"Reply with the single word: ready"}}' | timeout 90 agy --input-format stream-json --output-format stream-json > "$o/turn.out" 2> "$o/turn.err"; echo $? > "$o/turn.rc"
if command -v curl > /dev/null; then
  curl -sS -o /dev/null -w '%{http_code}\n' --max-time 20 https://cloudcode-pa.googleapis.com/ > "$o/https" 2> "$o/https.err"; echo $? > "$o/https.rc"
  curl -sS -o /dev/null -w '%{http_code}\n' --max-time 10 http://cloudcode-pa.googleapis.com/ > "$o/http80" 2> "$o/http80.err"; echo $? > "$o/http80.rc"
  # TCP 443 to the name's IPv4 address as the host resolved it (/workspace/ip4), no lookup asked:
  # egress itself, apart from DNS. And the same to an IPv6 address of the name, as found then.
  ip4=$(cat /workspace/ip4 2>/dev/null); ip6=$(cat /workspace/ip6 2>/dev/null)
  curl -sS -o /dev/null -w '%{http_code}\n' --max-time 10 --resolve "cloudcode-pa.googleapis.com:443:$ip4" https://cloudcode-pa.googleapis.com/ > "$o/https-ip4" 2> "$o/https-ip4.err"; echo $? > "$o/https-ip4.rc"
  curl -sS -o /dev/null -w '%{http_code}\n' --max-time 10 --resolve "cloudcode-pa.googleapis.com:443:[$ip6]" https://cloudcode-pa.googleapis.com/ > "$o/https-ip6" 2> "$o/https-ip6.err"; echo $? > "$o/https-ip6.rc"
else
  echo "no curl in this base" > "$o/https.err"
fi
ls -la /etc/ssl/certs/ca-certificates.crt > "$o/ca-bundle" 2>&1
# agy's own TLS: as it ends, it posts its telemetry over HTTPS; its log names what failed.
grep -a -E 'telemetry|x509|certificate' "$HOME/.gemini/antigravity-cli/cli.log" > "$o/agy-tls" 2>&1
ls -la "$HOME" > "$o/home.ls" 2>&1
echo done > "$o/done"
