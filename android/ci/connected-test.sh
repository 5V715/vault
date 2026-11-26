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
#
# With the stand-in, a real process death follows the Gradle run (which
# excludes ProcessDeathTest): its setup phase leaves a running session, this
# script kills the app process with kill -9, checks that the browser saw the
# session drop and that the system did not bring the service back, then runs
# the verify phase (process_death_locks: no notification, no session, a new
# pairing needs a new prompt) in a new process.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
adb logcat -c || true
extra=()
root=""
if [ -n "${VAULT_STAND_IN:-}" ]; then
  read -r relay_port control_port <<<"$VAULT_STAND_IN"
  # The process-death check kills the app with kill -9, which needs root
  # (run-as can't on API 30: "unknown pid", CI run 45742058742); the emulator
  # images are rootable. adbd restarts as root here, before the reverse
  # forwards exist (a restart later drops them: CI run 45744117861).
  echo "adb root: $(adb root 2>&1)"
  adb wait-for-device
  for _ in $(seq 30); do
    [ "$(adb shell id -u 2>/dev/null | tr -d '\r')" = 0 ] && { root=1; break; }
    sleep 1
  done
  echo "adb shell runs as root: ${root:-no}"
  for p in "$relay_port" "$control_port"; do
    adb reverse "tcp:$p" "tcp:$p" || { echo "connected-test: adb reverse tcp:$p failed" >&2; exit 1; }
  done
  adb reverse --list
  extra+=("-Pandroid.testInstrumentationRunnerArguments.standIn=$control_port")
fi
death_class=dev.vault.android.pairing.ProcessDeathTest
extra+=("-Pandroid.testInstrumentationRunnerArguments.notClass=$death_class")
ci/gradle.sh --stacktrace "${extra[@]}" "$@"
rc=$?

pkg=dev.vault.android
runner=dev.vault.android.test/androidx.test.runner.AndroidJUnitRunner
death_results=()
death_fail() {
  echo "connected-test: process death: $*" >&2
  death_results+=("FAILED   $death_class (host-driven kill -9): $*")
  return 1
}
# The setup phase, a kill -9 of the app process, the host-side checks, then
# the verify phase. Uses the stand-in's control API on 127.0.0.1:$control_port.
process_death() {
  local props=() a pid state out
  for a in "$@"; do [[ $a == -P* ]] && props+=("$a"); done
  ci/gradle.sh --stacktrace "${props[@]}" :app:installDebug :app:installDebugAndroidTest || { death_fail "install failed"; return 1; }
  adb shell am instrument -w -e standIn "$control_port" -e processDeathPhase setup \
    -e class "$death_class#process_death_setup" "$runner" > build/process-death-setup.txt 2>&1 &
  local inst=$!
  local ready=""
  for _ in $(seq 150); do
    [ "$(curl -fsS "http://127.0.0.1:$control_port/get?k=phase" 2>/dev/null)" = ready ] && { ready=1; break; }
    kill -0 "$inst" 2>/dev/null || break
    sleep 1
  done
  if [ -z "$ready" ]; then
    wait "$inst"; cat build/process-death-setup.txt
    death_fail "the setup phase never reported a running session"; return 1
  fi
  pid="$(adb shell pidof "$pkg" | tr -d '\r')"
  [ -n "$pid" ] || { death_fail "no process for $pkg"; return 1; }
  echo "process death: kill -9 $pid ($pkg)"
  if [ -n "$root" ]; then
    adb shell kill -9 "$pid" || { death_fail "kill -9 failed"; return 1; }
  else
    adb shell run-as "$pkg" kill -9 "$pid" || { death_fail "kill -9 failed (no root)"; return 1; }
  fi
  wait "$inst"
  cat build/process-death-setup.txt
  # The setup test only ends by the kill: anything else is a failure.
  grep -q 'OK (' build/process-death-setup.txt && { death_fail "the setup phase finished instead of being killed"; return 1; }
  state="$(curl -sS "http://127.0.0.1:$control_port/state?until=closed&timeout_ms=30000")"
  echo "process death: browser state after the kill: $state"
  [[ $state == closed:* ]] || { death_fail "the browser still has the session ($state)"; return 1; }
  # Give the system time to (wrongly) restart the service.
  sleep 10
  out="$(adb shell dumpsys activity services "$pkg")"
  if grep -q 'SessionService' <<<"$out"; then
    echo "$out"; death_fail "SessionService came back after the process died"; return 1
  fi
  # The notification: process_death_locks asserts that the new process has
  # none active (dumpsys would also list archived ones of earlier tests).
  adb shell am instrument -w -e standIn "$control_port" -e processDeathPhase verify \
    -e class "$death_class#process_death_locks" "$runner" > build/process-death-verify.txt 2>&1
  cat build/process-death-verify.txt
  grep -q '^OK (1 test)' build/process-death-verify.txt || { death_fail "process_death_locks failed in the new process"; return 1; }
  death_results+=("passed   $death_class.process_death_setup (killed by the host, as intended)")
  death_results+=("passed   $death_class.process_death_locks (new process after kill -9)")
}
if [ "$rc" = 0 ] && [ -n "${VAULT_STAND_IN:-}" ]; then
  mkdir -p build
  echo "::group::process death (host-driven)"
  process_death "$@" || rc=1
  echo "::endgroup::"
fi

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
for r in "${death_results[@]}"; do echo "$r"; done
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
