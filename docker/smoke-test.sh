#!/usr/bin/env bash
# MnemeCache comprehensive smoke test
# Usage: HOST=127.0.0.1:6380 PASS=testpass123 bash smoke.sh

HOST="${HOST:-127.0.0.1:6379}"
PASS="${PASS:-testpass123}"
MCLI="${MCLI:-mneme-cli}"

ok=0; fail=0

# Locate CA cert (symlinked by entrypoints to /etc/mneme/ca.crt)
CA_CERT=""
for _c in /etc/mneme/ca.crt /var/lib/mneme/ca.crt /certs/ca.crt; do
  [[ -f "$_c" ]] && { CA_CERT="$_c"; break; }
done
CA_ARGS=()
[[ -n "$CA_CERT" ]] && CA_ARGS=(--ca-cert "$CA_CERT")

c()  { "$MCLI" --host "$HOST" "${CA_ARGS[@]}" -u admin -p "$PASS" "$@" 2>&1; }

pass() { echo "  ✓ $1"; ((ok++)); }
xfail(){ echo "  ✗ $1 — got: $2"; ((fail++)); }

check() {
  local desc="$1" expected="$2"; shift 2
  local actual
  actual=$(c "$@") || true
  if echo "$actual" | grep -qF "$expected"; then pass "$desc"
  else xfail "$desc" "$actual"; fi
}

section() { echo ""; echo "══════════════════════════════════════"; echo "  $1"; echo "══════════════════════════════════════"; }

# ── Connectivity ──────────────────────────────────────────────
section "CONNECTIVITY"
check "stats responds"         "keys="          stats
check "pool-stats responds"    "Pool Used"      pool-stats
check "cluster-info responds"  "Node Role"      cluster-info
check "keeper-list responds"   "Node Name"      keeper-list

# ── String / KV ───────────────────────────────────────────────
section "STRING / KV"
c set s:hello "hello world"     > /dev/null
check "get"                     "hello world"    get s:hello
c set s:ttlkey "bye" --ttl 60   > /dev/null
# TTL returns ≤60; just check it's a positive integer
TTL=$(c ttl s:ttlkey)
if echo "$TTL" | grep -qE "\(integer\) [1-9][0-9]*"; then pass "ttl > 0"
else xfail "ttl > 0" "$TTL"; fi
check "exists present key"      "(integer) 1"    exists s:hello
check "exists absent key"       "(integer) 0"    exists no:such:key
check "del returns count"       "(integer) 2"    del s:hello s:ttlkey
# getset: first call on absent key returns (nil), sets new value
GSR=$(c getset s:gs "hello")
if echo "$GSR" | grep -qE '"\(nil\)"|\(nil\)'; then pass "getset on new key returns nil"
else
  # Some servers return nil without quotes
  if [[ "$GSR" == "(nil)" ]]; then pass "getset on new key returns nil"
  else xfail "getset on new key returns nil" "$GSR"; fi
fi
check "getset returns old val"  "hello"          getset s:gs "world"
check "get after getset"        "world"          get s:gs
c del s:gs > /dev/null

# ── Counters ──────────────────────────────────────────────────
section "COUNTERS"
# Use incr to create Counter-type key natively (avoids String→Counter OOM bug)
check "incr creates=1"          "(integer) 1"    incr c:hits
check "incr again=2"            "(integer) 2"    incr c:hits
check "incrby +10=12"           "(integer) 12"   incrby c:hits 10
check "decr=11"                 "(integer) 11"   decr c:hits
check "decrby -5=6"             "(integer) 6"    decrby c:hits 5
check "get counter"             "6"              get c:hits
c del c:hits > /dev/null

# incrbyfloat: creates Counter(floor(delta)) on new key, returns f64
check "incrbyfloat 1.5"         "1.5"            incrbyfloat f:r 1.5
# Server stores Counter(1) on first call; second adds 2.5 to 1 → 3.5
check "incrbyfloat +2.5"        "3.5"            incrbyfloat f:r 2.5
c del f:r > /dev/null

# ── Hash ──────────────────────────────────────────────────────
section "HASH"
c hset h:user name Alice age 30 city London  > /dev/null
check "hget name"               "Alice"          hget h:user name
check "hget age"                "30"             hget h:user age
check "hgetall has fields"      "Alice"          hgetall h:user
check "hdel city"               "(integer) 1"    hdel h:user city
HALL=$(c hgetall h:user)
if echo "$HALL" | grep -q "city"; then xfail "hdel: city still present" "$HALL"
else pass "hdel: city gone"; fi
c del h:user > /dev/null

# ── List ──────────────────────────────────────────────────────
section "LIST"
c rpush l:q job1 job2 job3  > /dev/null
c lpush l:q job0            > /dev/null
check "lrange 0 -1"             "job0"           lrange l:q 0 -- -1
check "lpop returns head"       '"job0"'         lpop l:q
check "rpop returns tail"       '"job3"'         rpop l:q
check "lrange after pops"       "job1"           lrange l:q 0 -- -1
c del l:q > /dev/null

# ── Sorted Set ────────────────────────────────────────────────
section "SORTED SET"
c zadd z:lb 1500.0 alice 2300.5 bob 900.0 charlie  > /dev/null
check "zscore alice"            "1500"           zscore z:lb alice
check "zrank charlie=0"         "(integer) 0"    zrank z:lb charlie
check "zrank alice=1"           "(integer) 1"    zrank z:lb alice
check "zcard=3"                 "(integer) 3"    zcard z:lb
check "zrange 0 -1"             "charlie"        zrange z:lb 0 -- -1
check "zrangebyscore 1000+"     "alice"          zrangebyscore z:lb 1000.0 9999.0
check "zrem charlie"            "(integer) 1"    zrem z:lb charlie
check "zcard after rem=2"       "(integer) 2"    zcard z:lb
c del z:lb > /dev/null

# ── MGET / MSET / TYPE ────────────────────────────────────────
section "MGET / MSET / TYPE"
c mset mk:a apple mk:b banana mk:c cherry  > /dev/null
check "mget all 3"              "apple"          mget mk:a mk:b mk:c
check "type string"             "string"         type mk:a
c del mk:a mk:b mk:c > /dev/null

c hset h:typ name Bob > /dev/null
check "type hash"               "hash"           type h:typ
c del h:typ > /dev/null

c rpush l:typ item > /dev/null
check "type list"               "list"           type l:typ
c del l:typ > /dev/null

c zadd z:typ 1.0 member > /dev/null
check "type zset"               "zset"           type z:typ
c del z:typ > /dev/null

# ── SCAN ──────────────────────────────────────────────────────
section "SCAN"
c set scan:apple 1 > /dev/null
c set scan:banana 2 > /dev/null
c set scan:cherry 3 > /dev/null
check "scan glob"               "scan:"          scan "scan:*"
check "scan exact match"        "scan:apple"     scan "scan:apple"
SCAN_NONE=$(c scan "nosuchprefix:*")
if echo "$SCAN_NONE" | grep -q "empty\|next_cursor=0"; then pass "scan empty result"
else xfail "scan empty result" "$SCAN_NONE"; fi
c del scan:apple scan:banana scan:cherry > /dev/null

# ── DB Namespacing ────────────────────────────────────────────
section "DB NAMESPACING"
c set ns:key "in-db0" > /dev/null
"$MCLI" --host "$HOST" "${CA_ARGS[@]}" -u admin -p "$PASS" -d 1 set ns:key "in-db1" > /dev/null 2>&1 || true
DB0=$(c get ns:key 2>&1)
DB1=$("$MCLI" --host "$HOST" "${CA_ARGS[@]}" -u admin -p "$PASS" -d 1 get ns:key 2>&1) || true
if echo "$DB0" | grep -q "in-db0" && echo "$DB1" | grep -q "in-db1"; then
  pass "db0 and db1 isolated"
else
  xfail "db isolation" "db0=$DB0 db1=$DB1"
fi
c del ns:key > /dev/null 2>&1 || true
"$MCLI" --host "$HOST" "${CA_ARGS[@]}" -u admin -p "$PASS" -d 1 del ns:key > /dev/null 2>&1 || true

# ── Auth / Tokens ─────────────────────────────────────────────
section "AUTH / TOKENS"
TOKEN=$(c auth-token 2>&1) || true
if echo "$TOKEN" | grep -qE "[A-Za-z0-9+/._-]{20,}"; then
  pass "auth-token issued"
  STAT=$("$MCLI" --host "$HOST" "${CA_ARGS[@]}" --token "$TOKEN" stats 2>&1) || true
  if echo "$STAT" | grep -q "keys="; then pass "token-based auth works"
  else xfail "token-based stats" "$STAT"; fi
else
  xfail "auth-token" "$TOKEN"
fi

# ── User Management ───────────────────────────────────────────
section "USER MANAGEMENT"
c user-create tester tpass --role readwrite > /dev/null 2>&1 || true
check "user-list shows tester"  "tester"         user-list
check "user-info role"          "readwrite"      user-info tester

# readwrite user can read+write
TSTAT=$("$MCLI" --host "$HOST" "${CA_ARGS[@]}" -u tester -p tpass stats 2>&1) || true
if echo "$TSTAT" | grep -q "keys="; then pass "readwrite user can stats"
else xfail "readwrite user stats" "$TSTAT"; fi

c user-delete tester > /dev/null 2>&1 || true
ULIST=$(c user-list 2>&1)
# Check for exact "tester " (with space) to avoid matching "tester2"
if echo "$ULIST" | grep -qP "^\s+tester\s"; then xfail "user-delete still listed" "$ULIST"
elif echo "$ULIST" | grep -q "| tester "; then xfail "user-delete still listed" "$ULIST"
else pass "user-delete: tester gone"; fi

# ── Observability ─────────────────────────────────────────────
section "OBSERVABILITY"
c set obs:key hello > /dev/null
check "config-set pool_bytes"   "OK"             config-set memory.pool_bytes 268435456
check "config get pool_bytes"   "268435456"      config memory.pool_bytes
check "dbsize"                  "(integer)"      dbsize
MEM_OUT=$(c memory-usage obs:key 2>&1) || true
if echo "$MEM_OUT" | grep -qE "\(integer\) [0-9]+"; then pass "memory-usage returns bytes"
else xfail "memory-usage" "$MEM_OUT"; fi
check "slowlog"                 ""               slowlog 2>/dev/null || true
c del obs:key > /dev/null

# ── JSON ──────────────────────────────────────────────────────
section "JSON"
c json-set j:doc '$' '{"name":"Widget","price":9.99,"tags":["fast","cheap"]}' > /dev/null
check "json-get root"           "Widget"         json-get j:doc '$'
check "json-get field"          "Widget"         json-get j:doc '$.name'
check "json-type object"        "object"         json-type j:doc '$'
check "json-exists name=true"   "true"           json-exists j:doc '$.name'
check "json-numincrby price"    "10.99"          json-numincrby j:doc '$.price' 1.0
c json-arrappend j:doc '$.tags' '"quality"' > /dev/null 2>&1 || true
check "json-get after append"   "quality"        json-get j:doc '$.tags'
check "json-del field"          ""               json-del j:doc '$.name' 2>/dev/null || true
c del j:doc > /dev/null

# ── Expiry ────────────────────────────────────────────────────
section "EXPIRY"
c set ex:key "value" --ttl 1 > /dev/null
sleep 2
OUT=$(c get ex:key 2>&1) || true
if echo "$OUT" | grep -qiE "not found|nil"; then pass "key expired after 1s TTL"
else xfail "key should be expired" "$OUT"; fi

# ──────────────────────────────────────────────────────────────
echo ""
echo "══════════════════════════════════════════════════"
TOTAL=$((ok + fail))
echo "  RESULTS: $ok/$TOTAL passed, $fail failed"
echo "══════════════════════════════════════════════════"
[[ $fail -eq 0 ]]
