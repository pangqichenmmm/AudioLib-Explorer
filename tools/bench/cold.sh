#!/bin/zsh
# OS-cold bench: `purge` before EVERY sample, s8 (baseline) vs s9 (option B), ROUNDS x RUNS per query.
# Gate: per query, median over rounds of per-round p95; PASS if s9 <= s8 * 1.2.
# Env: BIN (search-b-bench binary), DB, ROUNDS(5) RUNS(10), QUERIES (newline list; default = binary's built-in list),
#      SIDES ("s8 s9"), T3(1), DRY_RUN=1 (skip purge, self-test), OUT (log path)
#      BENCH_PREPARE_OUTSIDE_TIMER=1 (passed through to cold1 for s8 AND s9: FTS statements prepared on each new
#      connection before the timer, modelling pooled server readers; purge still runs before every sample)
# Auth: tries `sudo -n /usr/sbin/purge` (NOPASSWD sudoers rule => unattended); else `sudo -v` once + keepalive.
set -u
HERE=${BENCH_DIR:-$PWD}   # dir holding bench.db; results are written here
export MEMSTATUS0=1
BIN=${BIN:?set BIN to the search-b-bench binary}
DB=${DB:-$HERE/bench.db}; ROUNDS=${ROUNDS:-5}; RUNS=${RUNS:-10}; SIDES=${SIDES:-"s8 s9"}; DRY_RUN=${DRY_RUN:-0}
OUT=${OUT:-$HERE/cold-$(date +%Y%m%d-%H%M%S).log}; export T3=${T3:-1}
[[ -z ${QUERIES:-} ]] && QUERIES="$($BIN qs $DB)"
PURGE=(sudo -n /usr/sbin/purge); KEEP=""
if [[ $DRY_RUN == 1 ]]; then PURGE=(true); echo "DRY_RUN: purge skipped (results are NOT OS-cold)" | tee -a $OUT
elif sudo -n /usr/sbin/purge 2>/dev/null; then echo "auth: sudo -n (NOPASSWD) ok" | tee -a $OUT
else
  echo "auth: sudo -n failed -> sudo -v (enter password once)"; sudo -v || { echo "sudo -v failed; abort" | tee -a $OUT; exit 2; }
  ( while true; do sudo -n -v 2>/dev/null; sleep 50; done ) & KEEP=$!; trap '[[ -n $KEEP ]] && kill $KEEP 2>/dev/null' EXIT
  PURGE=(sudo -n /usr/sbin/purge); echo "auth: sudo -v ok, keepalive pid=$KEEP" | tee -a $OUT
fi
[[ $(pgrep -f 'search-b-bench (warm|cold|page)' | wc -l) -gt 0 ]] && { echo "another bench is running; abort" | tee -a $OUT; exit 3; }
echo "BIN=$BIN DB=$DB ROUNDS=$ROUNDS RUNS=$RUNS SIDES=$SIDES T3=$T3 BENCH_PREPARE_OUTSIDE_TIMER=${BENCH_PREPARE_OUTSIDE_TIMER:-0}" | tee -a $OUT
RAW=$OUT.raw.tsv; : > $RAW
echo "$QUERIES" | while IFS= read -r q; do [[ -z $q ]] && continue
  for r in $(seq 1 $ROUNDS); do for i in $(seq 1 $RUNS); do for s in ${=SIDES}; do
    $PURGE || { echo "purge failed" | tee -a $OUT; exit 4; }
    ms=$(Q="$q" ONLY=$s $BIN cold1 $DB | awk -F'ms=' '/^COLD1/{print $2}')
    printf '%s\t%s\t%s\t%s\n' "$q" $r $s $ms >> $RAW
  done; done; done
done
python3 - "$RAW" <<'PY' | tee -a $OUT
import sys,collections,statistics as st
d=collections.defaultdict(list)
for l in open(sys.argv[1]):
    q,r,s,ms=l.rstrip('\n').split('\t'); d[(q,s,r)].append(float(ms))
def p(v,x): v=sorted(v); return v[min(len(v)-1,int(round(x*(len(v)-1))))]
qs=list(dict.fromkeys(k[0] for k in d))
print("query\ts8_p50\ts8_p95\ts9_p50\ts9_p95\tratio\tgate(<=1.2x)")
for q in qs:
    m={}
    for s in ("s8","s9"):
        rs=[k[2] for k in d if k[0]==q and k[1]==s]
        if rs: m[s]=(st.median([p(d[(q,s,r)],.5) for r in rs]), st.median([p(d[(q,s,r)],.95) for r in rs]))
    if len(m)==2:
        ra=m["s9"][1]/m["s8"][1] if m["s8"][1] else float('inf')
        for s in ("s8","s9"):
            rs=sorted({k[2] for k in d if k[0]==q and k[1]==s}, key=int)
            print(f"ROUNDS\t{q}\t{s}_round_p95=[{','.join(f'{p(d[(q,s,r)],.95):.2f}' for r in rs)}]")
        print(f"{q}\t{m['s8'][0]:.2f}\t{m['s8'][1]:.2f}\t{m['s9'][0]:.2f}\t{m['s9'][1]:.2f}\t{ra:.3f}\t{'PASS' if ra<=1.2 else 'FAIL'}")
    else: print(q, m)
PY
echo "DONE log=$OUT raw=$RAW"
