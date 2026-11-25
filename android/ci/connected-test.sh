#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Runs the instrumented tests on the connected emulator and, if they fail,
# prints what explains the failure into the job log: each failed test's stack
# trace from the XML results and the relevant logcat lines (also saved to
# build/connected-logcat.txt for the uploaded artifacts). Arguments go to Gradle
# (android/ci/gradle.sh), e.g. -Pvault.abis=x86_64 :ffi:connectedDebugAndroidTest.
#
# VAULT_STAND_IN="<relay port> <control port>": the browser stand-in
# (vault-client/examples/browser_stand_in.rs) runs on the host on those ports.
# Both are forwarded into the emulator with `adb reverse`, so the app reaches
# them on its own 127.0.0.1 (the only host plain ws:// is allowed for), and the
# control port goes to the pairing tests as the instrumentation argument standIn.
# VAULT_STAND_IN_LOG: its log, printed on failure.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
adb logcat -c || true
extra=()
if [ -n "${VAULT_STAND_IN:-}" ]; then
  read -r relay_port control_port <<<"$VAULT_STAND_IN"
  for p in "$relay_port" "$control_port"; do
    adb reverse "tcp:$p" "tcp:$p" || { echo "connected-test: adb reverse tcp:$p failed" >&2; exit 1; }
  done
  extra+=("-Pandroid.testInstrumentationRunnerArguments.standIn=$control_port")
fi
ci/gradle.sh --stacktrace "${extra[@]}" "$@"
rc=$?

# Every instrumented test's result, passed ones included (the job log is the
# record of which tests ran; skipped ones are not passes).
echo "::group::instrumented test results"
python3 - <<'PY'
import glob, xml.etree.ElementTree as ET
for f in sorted(glob.glob("*/build/outputs/androidTest-results/connected/**/*.xml", recursive=True)):
    for case in ET.parse(f).getroot().iter("testcase"):
        if case.find("skipped") is not None:
            status = "SKIPPED"
        elif case.find("failure") is not None or case.find("error") is not None:
            status = "FAILED"
        else:
            status = "passed"
        print(f"{status:8} {case.get('classname')}.{case.get('name')} ({case.get('time')} s)")
PY
echo "::endgroup::"
[ "$rc" = 0 ] && exit 0

if [ -n "${VAULT_STAND_IN_LOG:-}" ] && [ -f "$VAULT_STAND_IN_LOG" ]; then
  echo "::group::browser stand-in log"
  tail -n 200 "$VAULT_STAND_IN_LOG"
  echo "::endgroup::"
fi

echo "::group::failed instrumented tests (stack traces)"
python3 - <<'PY'
import glob, xml.etree.ElementTree as ET
files = glob.glob("*/build/outputs/androidTest-results/connected/**/*.xml", recursive=True)
print(f"result files: {files}")
for f in files:
    for case in ET.parse(f).getroot().iter("testcase"):
        for bad in list(case.iter("failure")) + list(case.iter("error")):
            print(f"=== {case.get('classname')}.{case.get('name')} ({f})")
            print(bad.text or bad.get("message") or "")
PY
echo "::endgroup::"

mkdir -p build
adb logcat -d -v threadtime > build/connected-logcat.txt 2>&1 || true
echo "::group::logcat (test process, crashes, linker, JNA/UniFFI)"
pids="$(grep -E 'Start proc [0-9]+:dev\.vault' build/connected-logcat.txt | sed -E 's/.*Start proc ([0-9]+):.*/\1/' | sort -u | tr '\n' '|')"
pids="${pids%|}"
pattern='FATAL|AndroidRuntime|linker|[Uu]ni[Ff][Ff][Ii]|JNA|jnidispatch|vault|TestRunner|System\.err|DEBUG +:'
[ -n "$pids" ] && pattern="$pattern|^[^ ]+ [^ ]+ +($pids) "
grep -E "$pattern" build/connected-logcat.txt | tail -n 400 || true
echo "::endgroup::"
exit "$rc"
