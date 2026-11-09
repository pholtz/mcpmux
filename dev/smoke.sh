#!/bin/sh
# End-to-end check of the daemon against dev/fake-mcp.py, under a scratch HOME.
# Usage: dev/smoke.sh   (builds debug, runs on 127.0.0.1:7899)
set -u
here=$(cd "$(dirname "$0")/.." && pwd)
cargo build -q --manifest-path "$here/Cargo.toml" || exit 1
bin=$here/target/debug/mcpmux
export HOME=$(mktemp -d /tmp/mcpmux-smoke.XXXX)
mkdir -p "$HOME/.config/mcpmux"
sed "s|\"python3\"|\"/usr/bin/python3\"|; s|FAKE|$here/dev/fake-mcp.py|" "$here/dev/test.toml" > "$HOME/.config/mcpmux/config.toml"
head -c 32 /dev/urandom | xxd -p -c 64 > "$HOME/.config/mcpmux/token"
TOKEN=$(cat "$HOME/.config/mcpmux/token")
"$bin" serve 2> "$HOME/daemon.log" &
daemon=$!
trap 'kill $daemon 2>/dev/null; wait $daemon 2>/dev/null' EXIT
for _ in $(seq 50); do [ -S "$HOME/.local/state/mcpmux/admin.sock" ] && break; sleep 0.1; done
U=http://127.0.0.1:7899/fake/mcp
fail=0
ok() { printf '  ok   %s\n' "$1"; }
bad() { printf '  FAIL %s\n' "$1"; fail=1; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }
post() { rawpost "$1" | sed -n -e 's/^data: //p' -e t -e '/^{/p'; }
rawpost() { curl -s -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' -H "Mcp-Session-Id: ${SID:-s1}" "$U" -d "$1"; }
children() { pgrep -f fake-mcp.py | wc -l | tr -d ' '; }
grandkids() { pgrep -f '^sleep 3600$' | wc -l | tr -d ' '; }

echo "auth"
check "no token → 401" '[ "$(curl -s -o /dev/null -w %{http_code} -X POST $U -d {})" = 401 ]'
check "bad Host → 403" '[ "$(curl -s -o /dev/null -w %{http_code} -H "Host: evil.com" -H "Authorization: Bearer $TOKEN" -X POST $U -d {})" = 403 ]'
check "browser Origin → 403" '[ "$(curl -s -o /dev/null -w %{http_code} -H "Origin: http://evil.com" -H "Authorization: Bearer $TOKEN" -X POST $U -d {})" = 403 ]'

echo "first handshake (no cache) spawns, then idle"
r=$(post '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}')
check "initialize ok" '[ "$(echo "$r" | jq -r .result.serverInfo.name)" = fake ]'
names=$(post '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | jq -c '[.result.tools[].name]')
check "deny_tools hides connect" '[ "$names" = "[\"echo\",\"sleep\",\"crash\"]" ]'
check "denied call refused" 'post "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"connect\"}}" | jq -e ".error" >/dev/null'
"$bin" stop fake >/dev/null
check "stop kills child and grandchild" '[ "$(children)" = 0 ] && [ "$(grandkids)" = 0 ]'

echo "lazy: cached handshake spawns nothing"
post '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' >/dev/null
post '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' >/dev/null
check "no child after initialize + tools/list" '[ "$(children)" = 0 ]'

echo "concurrency: 4 sessions × parallel calls share one child"
for s in a b c d; do
  SID=$s post '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"sleep","arguments":{"secs":1}}}' > "$HOME/out.$s" &
done
sleep 0.6
check "exactly one child" '[ "$(children)" = 1 ]'
wait %2 %3 %4 %5 2>/dev/null
check "every caller got its own reply with id 7" '[ "$(cat $HOME/out.? | jq -s "map(select(.id==7 and .result)) | length")" = 4 ]'

echo "progress over SSE"
r=$(rawpost '{"jsonrpc":"2.0","id":"p1","method":"tools/call","params":{"name":"sleep","arguments":{"secs":0.4},"_meta":{"progressToken":"tok-xyz"}}}')
check "4 progress events with the client's token" '[ "$(echo "$r" | grep -c "\"progressToken\":\"tok-xyz\"")" = 4 ]'
check "final result on the stream" 'echo "$r" | sed -n "s/^data: //p" | jq -e "select(.id==\"p1\") | .result" >/dev/null'

echo "client hang-up cancels upstream"
curl -s -o /dev/null -H "Authorization: Bearer $TOKEN" -H 'Accept: application/json, text/event-stream' \
  "$U" -d '{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"sleep","arguments":{"secs":14}}}' &
cpid=$!; sleep 0.3; kill $cpid; sleep 12  # noticed on a keep-alive write (5s interval)
check "child saw notifications/cancelled" 'grep -q "^cancelled" "$HOME/Library/Logs/mcpmux/fake.log"'

echo "crash mid-call"
r=$(post '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"crash"}}')
check "caller gets an error" 'echo "$r" | jq -e .error >/dev/null'
sleep 0.3
check "grandchild cleaned up after crash" '[ "$(grandkids)" = 0 ]'
r=$(post '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"echo","arguments":{"x":1}}}')
check "next call respawns" 'echo "$r" | jq -e .result >/dev/null'

echo "tunnel precondition"
r=$(curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7899/tunnel/mcp -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo"}}')
check "fails fast with tunnel hint" 'echo "$r" | jq -r .error.message | grep -q "tunnel up\`"'

echo "status"
"$bin" status | sed 's/^/    /'

echo "idle stop (20s)"
sleep 36
check "child stopped after idle timeout" '[ "$(children)" = 0 ] && [ "$(grandkids)" = 0 ]'

echo "daemon exit stops children"
post '{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"echo"}}' >/dev/null
kill $daemon; wait $daemon 2>/dev/null; sleep 0.3
check "nothing left running" '[ "$(children)" = 0 ] && [ "$(grandkids)" = 0 ]'
check "logs hold no tool arguments" '! grep -q "\"x\":1" "$HOME/daemon.log"'

[ $fail = 0 ] && echo PASS || { echo "FAILED; daemon log:"; cat "$HOME/daemon.log"; exit 1; }
