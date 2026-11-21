#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Runs the instrumented tests on the connected emulator and, if they fail,
# prints what explains the failure into the job log: each failed test's stack
# trace from the XML results and the relevant logcat lines (also saved to
# build/connected-logcat.txt for the uploaded artifacts). Arguments go to Gradle
# (android/ci/gradle.sh), e.g. -Pvault.abis=x86_64 :ffi:connectedDebugAndroidTest.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
adb logcat -c || true
ci/gradle.sh --stacktrace "$@"
rc=$?
[ "$rc" = 0 ] && exit 0

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
