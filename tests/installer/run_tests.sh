#!/bin/bash
# Offline tests for install.sh: mock curl/ha/cargo/git/uname; nothing is
# written to the real HOME and all fixtures live in temp directories.
set -u
HERE="$(cd -P "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_SH="$HERE/../../install.sh"
PASS=0
FAIL=0

FAKE_HOME=""
BIN_PATH=""
FIX=""
PORT=""

cleanup() {
	[ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null
	[ -n "$FAKE_HOME" ] && rm -rf "$FAKE_HOME"
	[ -n "$BIN_PATH" ] && rm -rf "$BIN_PATH"
	[ -n "$FIX" ] && rm -rf "$FIX"
}
trap cleanup EXIT

# assert <name> <cmd...>  — command must succeed
assert() {
	local name="$1"; shift
	if "$@" >/dev/null 2>&1; then
		PASS=$((PASS + 1)); printf 'ok   %s\n' "$name"
	else
		FAIL=$((FAIL + 1)); printf 'FAIL %s\n' "$name"
	fi
}

# assert_fail <name> <cmd...> — command must fail
assert_fail() {
	local name="$1"; shift
	if "$@" >/dev/null 2>&1; then
		FAIL=$((FAIL + 1)); printf 'FAIL %s (unexpectedly succeeded)\n' "$name"
	else
		PASS=$((PASS + 1)); printf 'ok   %s\n' "$name"
	fi
}

# assert_out <name> <pattern> <cmd...> — command output must match pattern
# (regardless of exit status, so failing installs can be checked too)
	assert_out() {
		local name="$1" pattern="$2"; shift 2
		local out
		out="$("$@" 2>&1)" || true
		if printf '%s' "$out" | grep -qE "$pattern"; then
		PASS=$((PASS + 1)); printf 'ok   %s\n' "$name"
	else
		FAIL=$((FAIL + 1)); printf 'FAIL %s\n' "$name"
	fi
}

make_tarball() {
	# make_tarball FIXDIR TAG TARGET VERSION [ha-script-body]
	local dest="$1" tag="$2" target="$3" ver="$4" body="${5:-}"
	mkdir -p "$dest/ha-$tag-$target"
	if [ -n "$body" ]; then
		printf '%s\n' "$body" >"$dest/ha-$tag-$target/ha"
	else
		cat >"$dest/ha-$tag-$target/ha" <<EOF
#!/bin/bash
case "\$1" in
	--version) echo "ha-cli $ver" ;;
	--help) echo "usage: ha" ;;
	*) echo "ha" ;;
esac
EOF
	fi
	chmod +x "$dest/ha-$tag-$target/ha"
	tar -czf "$dest/ha-$tag-$target.tar.gz" -C "$dest" "ha-$tag-$target"
	# sha256 file references dist/<name>, as in the real release assets.
	h="$(sha256sum "$dest/ha-$tag-$target.tar.gz" | awk '{print $1}')"
	printf '%s  dist/%s\n' "$h" "ha-$tag-$target.tar.gz" >"$dest/ha-$tag-$target.tar.gz.sha256"
}

make_skill_src_tarball() {
	# make_skill_src_tarball FIXDIR TAG
	local dest="$1" tag="$2"
	local root="$dest/ha_cli-$tag"
	mkdir -p "$root/skills/ha-control"
	cat >"$root/skills/ha-control/SKILL.md" <<'EOF'
# ha-control
Run: {{HA_BIN}} --help
EOF
	printf 'name = "ha-control"\n' >"$root/skills/ha-control/SKILL.toml"
	tar -czf "$dest/$tag.tar.gz" -C "$dest" "ha_cli-$tag"
	h="$(sha256sum "$dest/$tag.tar.gz" | awk '{print $1}')"
	printf '%s  dist/%s\n' "$h" "$tag.tar.gz" >"$dest/$tag.tar.gz.sha256"
	rm -rf "$root"
}

setup() {
	FAKE_HOME="$(mktemp -d "${TMPDIR:-/tmp}/ha-inst-home.XXXXXXXX")"
	BIN_PATH="$(mktemp -d "${TMPDIR:-/tmp}/ha-inst-bin.XXXXXXXX")"
	FIX="$(mktemp -d "${TMPDIR:-/tmp}/ha-inst-fix.XXXXXXXX")"
	mkdir -p "$FIX/dl"
	cat >"$FIX/api.json" <<EOF
{"tag_name": "v0.2.0", "assets": []}
EOF
	make_tarball "$FIX/dl" v0.2.0 x86_64-unknown-linux-musl 0.2.0
	make_tarball "$FIX/dl" v0.2.0 aarch64-unknown-linux-musl 0.2.0
	make_skill_src_tarball "$FIX/dl" v0.2.0
	cat >"$BIN_PATH/curl" <<EOF
#!/bin/bash
exec "$HERE/mock_curl" "$FIX" "http://127.0.0.1:$PORT" "\$@"
EOF
	chmod +x "$BIN_PATH/curl"
	export HOME="$FAKE_HOME"
}

run_install() {
	( cd "$BIN_PATH" && PATH="$BIN_PATH:$PATH" bash "$INSTALL_SH" "$@" )
}

serve_fixtures() {
	PORT=""
	PORT="$(python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)"
	python3 - "$PORT" "$FIX" <<'PY' &
import http.server, sys, functools
class H(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *a):
        pass
handler = functools.partial(H, directory=sys.argv[2])
http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), handler).serve_forever()
PY
	SERVER_PID=$!
	for _ in $(seq 1 50); do
		curl -fsS "http://127.0.0.1:$PORT/api.json" >/dev/null 2>&1 && break
		sleep 0.1
	done
}

set_mock_uname() {
	# set_mock_uname <arch> [os]
	cat >"$BIN_PATH/uname" <<EOF
#!/bin/bash
case "\${1:-}" in
	-m) echo "$1" ;;
	-s) echo "${2:-Linux}" ;;
	*) echo "${2:-Linux} $1" ;;
esac
EOF
	chmod +x "$BIN_PATH/uname"
}

set_mock_cargo_git() {
	# Fake cargo (emits a working ha binary) and git (clone from FIX/repo).
	cat >"$BIN_PATH/cargo" <<'EOF'
#!/bin/bash
while [ $# -gt 0 ]; do
	case "$1" in
	--manifest-path) SRC="$(dirname "$2")"; shift 2 ;;
	*) shift ;;
	esac
done
mkdir -p "$SRC/target/release"
cat >"$SRC/target/release/ha" <<'INNER'
#!/bin/bash
case "$1" in
	--version) echo "ha-cli 0.2.0" ;;
	*) echo "usage: ha" ;;
esac
INNER
	chmod +x "$SRC/target/release/ha"
EOF
	cat >"$BIN_PATH/git" <<EOF
#!/bin/bash
while [ \$# -gt 0 ]; do
	case "\$1" in
	clone) DEST="\${@: -1}"; mkdir -p "\$DEST"; cp -r "$FIX/repo/." "\$DEST/"; exit 0 ;;
	*) shift ;;
	esac
done
EOF
	chmod +x "$BIN_PATH/cargo" "$BIN_PATH/git"
	mkdir -p "$FIX/repo"
	touch "$FIX/repo/Cargo.toml"
	mkdir -p "$FIX/repo/skills/ha-control"
	printf 'Run: {{HA_BIN}}\n' >"$FIX/repo/skills/ha-control/SKILL.md"
	printf 'name = "ha-control"\n' >"$FIX/repo/skills/ha-control/SKILL.toml"
}

# unit-test version helpers by extracting the marked block from install.sh
version_unit_tests() {
	local lib
	lib="$(mktemp)"
	sed -n '/^# --- version helpers ---/,/^# --- end version helpers ---/p' "$INSTALL_SH" >"$lib"
	# shellcheck disable=SC1090
	(
		set +e
		# shellcheck disable=SC1090
		source "$lib"
		t() { # t <yes|no> A B — yes if version_ge A B must be true
			if version_ge "$2" "$3"; then got=yes; else got=no; fi
			[ "$got" = "$1" ]
		}
		t no 0.2.0-rc1 0.2.0 || exit 1
		t yes 0.2.0 0.2.0-rc1 || exit 2
		t yes 0.2.0 0.2.0 || exit 3
		t no 0.1.8 0.1.10 || exit 4   # octal trap (08)
		t yes 0.10.0 0.9.0 || exit 5
		t yes 1.0.0 0.99.99 || exit 6
		t no 0.1.0 0.2.0 || exit 7
		t yes 0.2.0-rc2 0.2.0-rc1 || exit 8
		t no 0.2.0-rc1 0.2.0-rc2 || exit 9
		# strict numeric validation: invalid input -> return 2 (false)
		version_ge 0.1.0 banana && exit 11
		version_ge banana 0.1.0 && exit 12
		version_ge 01.2.0 1.2.0 || exit 13 # leading zeros still numeric
		version_ge 1.2.3.4 1.2.3 && exit 14
		version_ge 1.2.3- 1.2.3 && exit 15
		exit 0
	)
	local rc=$?
	rm -f "$lib"
	if [ "$rc" -eq 0 ]; then
		PASS=$((PASS + 1)); printf 'ok   version_ge unit tests\n'
	else
		FAIL=$((FAIL + 1)); printf 'FAIL version_ge unit tests (case rc=%s)\n' "$rc"
	fi
}

main() {
	version_unit_tests
	serve_fixtures

	# --- 1. fresh install, x86_64 ---
	setup
	assert "fresh install" run_install --install-dir "$FAKE_HOME/bin"
	assert "binary installed" test -x "$FAKE_HOME/bin/ha"
	assert "installed version is 0.2.0" bash -c "\"$FAKE_HOME/bin/ha\" --version | grep -q 0.2.0"
	assert "no skill without --skills-root" bash -c "! test -e '$FAKE_HOME/skills'"
	cleanup; SERVER_PID=""

	# --- 2. fresh install, aarch64 (mock uname) ---
	setup
	set_mock_uname aarch64
	assert "aarch64 install" run_install --install-dir "$FAKE_HOME/bin"
	assert "aarch64 binary present" test -x "$FAKE_HOME/bin/ha"
	cleanup; SERVER_PID=""

	# --- 3. checksum mismatch: corrupt tarball, valid .sha256 ---
	setup
	printf 'broken' >>"$FIX/dl/ha-v0.2.0-x86_64-unknown-linux-musl.tar.gz"
	assert_out "checksum mismatch detected" "checksum mismatch" \
		run_install --install-dir "$FAKE_HOME/bin"
	assert_fail "no binary after checksum failure" test -e "$FAKE_HOME/bin/ha"
	cleanup; SERVER_PID=""

	# --- 4. skip when installed >= latest ---
	setup
	mkdir -p "$FAKE_HOME/bin"
	printf '#!/bin/bash\necho "ha-cli 9.9.9"\n' >"$FAKE_HOME/bin/ha"
	chmod +x "$FAKE_HOME/bin/ha"
	assert_out "skip message shown" "skipping binary install" \
		run_install --install-dir "$FAKE_HOME/bin"
	assert "old binary kept" bash -c "\"$FAKE_HOME/bin/ha\" --version | grep -q 9.9.9"
	cleanup; SERVER_PID=""

	# --- 4b. prerelease installed: 0.2.0-rc1 < 0.2.0 -> update, not skip ---
	setup
	mkdir -p "$FAKE_HOME/bin"
	printf '#!/bin/bash\necho "ha-cli 0.2.0-rc1"\n' >"$FAKE_HOME/bin/ha"
	chmod +x "$FAKE_HOME/bin/ha"
	assert_out "prerelease rc1 updates to stable" "updating ha" \
		run_install --install-dir "$FAKE_HOME/bin"
	assert "updated to 0.2.0 stable" bash -c "\"$FAKE_HOME/bin/ha\" --version | grep -qx 'ha-cli 0.2.0'"
	cleanup; SERVER_PID=""

	# --- 5. update from older version (incl. octal-trap 0.1.8-style) ---
	setup
	mkdir -p "$FAKE_HOME/bin"
	printf '#!/bin/bash\necho "ha-cli 0.1.08"\n' >"$FAKE_HOME/bin/ha"
	chmod +x "$FAKE_HOME/bin/ha"
	assert "update install" run_install --install-dir "$FAKE_HOME/bin"
	assert "binary updated to 0.2.0" bash -c "\"$FAKE_HOME/bin/ha\" --version | grep -q 0.2.0"
	cleanup; SERVER_PID=""

	# --- 6. rollback: old binary restored when new binary fails smoke test ---
	setup
	mkdir -p "$FAKE_HOME/bin"
	printf '#!/bin/bash\necho "ha-cli 0.1.0"\n' >"$FAKE_HOME/bin/ha"
	chmod +x "$FAKE_HOME/bin/ha"
	make_tarball "$FIX/dl" v0.2.0 x86_64-unknown-linux-musl 0.2.0 '#!/bin/bash
exit 1'
	assert_fail "smoke-test failure aborts install" run_install --install-dir "$FAKE_HOME/bin"
	assert "old binary restored after rollback" bash -c "\"$FAKE_HOME/bin/ha\" --version | grep -q 0.1.0"
	cleanup; SERVER_PID=""

	# --- 7. skill mode: files + {{HA_BIN}} substitution, & | \ in path ---
	setup
	HA_BIN_DIR="$FAKE_HOME/weird & | dir"
	mkdir -p "$HA_BIN_DIR"
	printf '#!/bin/bash\necho "ha-cli 0.2.0"\n' >"$HA_BIN_DIR/ha"
	chmod +x "$HA_BIN_DIR/ha"
	assert "skill install (path with & | space)" \
		run_install skill --skills-root "$FAKE_HOME/openclaw" --install-dir "$HA_BIN_DIR"
	assert "SKILL.md installed" test -f "$FAKE_HOME/openclaw/ha-control/SKILL.md"
	assert "SKILL.toml installed" test -f "$FAKE_HOME/openclaw/ha-control/SKILL.toml"
	assert "HA_BIN substituted verbatim" \
		grep -Fxq "Run: $HA_BIN_DIR/ha --help" "$FAKE_HOME/openclaw/ha-control/SKILL.md"
	# skill mode without binary: warns but succeeds
	rm "$HA_BIN_DIR/ha"
	assert_out "skill mode warns about missing binary" "binary not found" \
		run_install skill --skills-root "$FAKE_HOME/openclaw" --install-dir "$HA_BIN_DIR"
	assert "skill reinstalled despite missing binary" \
		test -f "$FAKE_HOME/openclaw/ha-control/SKILL.md"
	cleanup; SERVER_PID=""

	# --- 8. skill rollback: failure after old dir renamed aside restores it ---
	setup
	mkdir -p "$FAKE_HOME/openclaw/ha-control"
	echo "old skill" >"$FAKE_HOME/openclaw/ha-control/SKILL.md"
	HA_CLI_TEST_FAIL_POINT=after_skill_backup \
		assert_fail "skill install fails after old dir renamed aside" \
		run_install skill --skills-root "$FAKE_HOME/openclaw"
	assert "old skill preserved after failed skill install" \
		bash -c "grep -q 'old skill' '$FAKE_HOME/openclaw/ha-control/SKILL.md'"
	assert "no backup dirs left behind" \
		bash -c "! ls -d '$FAKE_HOME/openclaw'/.ha-control.bak.* 2>/dev/null"
	cleanup; SERVER_PID=""

	# --- 9. binary already OK is not removed when skill install fails ---
	setup
	assert "binary+skills install (prereq)" \
		run_install --install-dir "$FAKE_HOME/bin" --skills-root "$FAKE_HOME/skills"
	# break the skill source tarball so skill phase fails, binary stays
	printf 'junk' >>"$FIX/dl/v0.2.0.tar.gz"
	assert_fail "skill failure aborts run" \
		run_install --install-dir "$FAKE_HOME/bin" --skills-root "$FAKE_HOME/skills"
	assert "binary survived skill failure" \
		bash -c "\"$FAKE_HOME/bin/ha\" --version | grep -q 0.2.0"
	assert "old skill dir survived skill failure" \
		test -f "$FAKE_HOME/skills/ha-control/SKILL.md"
	cleanup; SERVER_PID=""

	# --- 10. args: help, unknown, relative/spaced paths, skills-root created ---
	setup
	assert "--help exits 0" run_install --help
	assert_fail "unknown arg fails" run_install --bogus
	assert_fail "skill mode without --skills-root fails" run_install skill
	assert "relative --install-dir works" \
		run_install --install-dir "rel-bin" --skills-root "$FAKE_HOME/sr"
	assert "binary in normalized relative dir" test -x "$BIN_PATH/rel-bin/ha"
	assert "skills-root created if missing" test -d "$FAKE_HOME/sr"
	assert "spaces in install-dir" run_install --install-dir "$FAKE_HOME/my bin dir"
	assert "binary in spaced dir" test -x "$FAKE_HOME/my bin dir/ha"
	cleanup; SERVER_PID=""

	# --- 11. build-from-source consent (no TTY before network; --force) ---
	setup
	set_mock_cargo_git
	assert_fail "no TTY without --force refuses" \
		run_install --install-dir "$FAKE_HOME/bin" --build-from-source
	assert_fail "no binary built without consent" test -x "$FAKE_HOME/bin/ha"
	assert "build-from-source --force works (clone path)" \
		run_install --install-dir "$FAKE_HOME/bin" --build-from-source --force
	assert "binary built and installed" test -x "$FAKE_HOME/bin/ha"
	assert "built binary version 0.2.0" bash -c "\"$FAKE_HOME/bin/ha\" --version | grep -q 0.2.0"
	cleanup; SERVER_PID=""

	# --- 11b. source mode + --skills-root installs skill from same source ---
	setup
	set_mock_cargo_git
	assert "source build with skills-root" \
		run_install --install-dir "$FAKE_HOME/bin" --skills-root "$FAKE_HOME/skills" \
			--build-from-source --force
	assert "skill installed from source tree" \
		bash -c "grep -q '$FAKE_HOME/bin/ha' '$FAKE_HOME/skills/ha-control/SKILL.md'"
	cleanup; SERVER_PID=""

	# --- 11c. local checkout: no git needed, no clone performed ---
	setup
	set_mock_cargo_git
	# a git that always fails: must not be invoked for a local checkout
	printf '#!/bin/bash\nexit 99\n' >"$BIN_PATH/git"
	chmod +x "$BIN_PATH/git"
	assert "local checkout build without git" \
		run_install --install-dir "$FAKE_HOME/bin" --build-from-source --force
	assert "checkout binary installed" test -x "$FAKE_HOME/bin/ha"
	cleanup; SERVER_PID=""

	# --- 12. unsupported architecture / OS errors ---
	setup
	set_mock_uname armv7l
	assert_out "unsupported arch fails with hint" "build-from-source" \
		run_install --install-dir "$FAKE_HOME/bin"
	cleanup; SERVER_PID=""

	setup
	set_mock_uname x86_64 Darwin
	assert_fail "unsupported OS fails" run_install --install-dir "$FAKE_HOME/bin"
	assert_out "unsupported OS mentions hint" "build-from-source" \
		run_install --install-dir "$FAKE_HOME/bin"
	cleanup; SERVER_PID=""

	# --- 13. malicious tarball paths refused ---
	setup
	TRAV="$FIX/dl/evil.tar.gz"
	mkdir -p "$FIX/evil/skills/ha-control"
	printf 'x' >"$FIX/evil/skills/ha-control/SKILL.md"
	tar -czf "$TRAV" -C "$FIX" --transform 's#^evil#../evil#' evil
	# point the skill tarball name at the evil archive
	cp "$TRAV" "$FIX/dl/v0.2.0.tar.gz"
	h="$(sha256sum "$FIX/dl/v0.2.0.tar.gz" | awk '{print $1}')"
	printf '%s  dist/%s\n' "$h" "v0.2.0.tar.gz" >"$FIX/dl/v0.2.0.tar.gz.sha256"
	assert_fail "traversal tarball refused" \
		run_install skill --skills-root "$FAKE_HOME/openclaw"
	assert "nothing written outside temp" bash -c "! test -e '$FAKE_HOME/../evil'"
	cleanup; SERVER_PID=""

	printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
	[ "$FAIL" -eq 0 ]
}

main "$@"
