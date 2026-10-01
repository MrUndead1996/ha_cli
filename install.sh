#!/bin/bash
# install.sh — installer for the `ha` CLI and the OpenClaw ha-control skill.
#
# Default mode installs a prebuilt musl binary from the latest GitHub
# release of MrUndead1996/ha_cli (Linux x86_64 / aarch64), verifying the
# published .sha256 checksum. Works via `curl ... | bash` without git/cargo.
#
# `install.sh skill --skills-root PATH` installs only the skill files from
# the release source tarball (no binary build), substituting the absolute
# `ha` path from --install-dir into SKILL.md.
#
# Explicit source build: --build-from-source (uses a nearby checkout when
# it contains Cargo.toml, otherwise clones into a temp dir after the user
# confirms; prompts before any network access unless --force is given).
#
# Config ~/.config/ha-cli/config.toml is never touched.

set -euo pipefail

REPO_SLUG="MrUndead1996/ha_cli"
API_URL="${HA_CLI_API_URL:-https://api.github.com/repos/${REPO_SLUG}/releases/latest}"
DL_BASE="${HA_CLI_DL_BASE:-https://github.com/${REPO_SLUG}/releases/download}"
SRC_BASE="${HA_CLI_SRC_BASE:-https://github.com/${REPO_SLUG}/archive/refs/tags}"
ASSET_URL_SUFFIX="${HA_CLI_ASSET_URL_SUFFIX:-}"

MODE="install"
INSTALL_DIR="${HOME:-}/.local/bin"
SKILLS_ROOT=""
SKILLS_ROOT_GIVEN=0
BUILD_FROM_SOURCE=0
FORCE=0

usage() {
	cat <<'EOF'
install.sh — install the ha CLI (and optionally the OpenClaw ha-control skill).

Usage:
  install.sh [options]              Install the `ha` binary from the latest
                                    GitHub release (Linux x86_64/aarch64).
  install.sh skill [options]        Install only the OpenClaw ha-control
                                    skill files from the latest release.
  install.sh --help                 Show this help.

Options:
  --install-dir PATH     Target directory for the `ha` binary
                         (default: ~/.local/bin). Created if missing.
  --skills-root PATH     Install the OpenClaw skill into PATH/ha-control
                         (only needed in default mode; `skill` mode
                         requires it). Created if missing.
  --build-from-source    Build from sources instead of downloading a
                         release binary. Prompts before cloning/building
                         unless --force is given.
  --force                Skip the confirmation prompt for
                         --build-from-source.
  -h, --help             Show this help.

Examples:
  # Install / update the binary:
  curl -fsSL https://raw.githubusercontent.com/MrUndead1996/ha_cli/main/install.sh | bash

  # Install only the skill (expects the binary in ~/.local/bin):
  install.sh skill --skills-root ~/.openclaw/skills

  # Both binary and skill:
  install.sh --skills-root ~/.openclaw/skills

  # Custom install location:
  install.sh --install-dir /usr/local/bin

  # Build from local sources without prompting:
  install.sh --build-from-source --force

Notes:
  * Release tarballs are checksum-verified against the published .sha256.
  * If the installed `ha` version is >= the latest release, the binary
    install is skipped.
  * Unsupported OS/architecture: rerun with --build-from-source.
  * Failed installs roll back: the previous binary/skill is restored.
EOF
}

log() { printf 'install.sh: %s\n' "$*"; }
die() { printf 'install.sh: error: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Argument parsing (unknown args fail).
# ---------------------------------------------------------------------------

parse_args() {
	if [ $# -gt 0 ] && [ "$1" = "skill" ]; then
		MODE="skill"
		shift
	fi
	while [ $# -gt 0 ]; do
		case "$1" in
		--install-dir)
			[ $# -ge 2 ] || die "--install-dir requires a PATH argument"
			INSTALL_DIR="$2"
			shift 2
			;;
		--skills-root)
			[ $# -ge 2 ] || die "--skills-root requires a PATH argument"
			SKILLS_ROOT="$2"
			SKILLS_ROOT_GIVEN=1
			shift 2
			;;
		--build-from-source)
			BUILD_FROM_SOURCE=1
			shift
			;;
		--force)
			FORCE=1
			shift
			;;
		-h | --help)
			usage
			exit 0
			;;
		*)
			printf 'install.sh: unknown argument: %s\n' "$1" >&2
			usage >&2
			exit 2
			;;
		esac
	done
}

parse_args "$@"

if [ -z "${HOME:-}" ] && [ "$INSTALL_DIR" = "${HOME:-}/.local/bin" ]; then
	die "HOME is not set; pass --install-dir PATH explicitly"
fi

need_cmd() { command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"; }

need_cmd curl
need_cmd tar
need_cmd sha256sum
need_cmd awk
need_cmd sed
need_cmd mktemp

# Normalize to an absolute physical path (creates the directory).
normalize_dir() {
	mkdir -p -- "$1" || return 1
	(cd -P -- "$1" >/dev/null 2>&1 && pwd) || return 1
}

INSTALL_DIR="$(normalize_dir "$INSTALL_DIR")" ||
	die "cannot create/access --install-dir: $INSTALL_DIR"
if [ -n "$SKILLS_ROOT" ]; then
	SKILLS_ROOT="$(normalize_dir "$SKILLS_ROOT")" ||
		die "cannot create/access --skills-root: $SKILLS_ROOT"
fi

# ---------------------------------------------------------------------------
# Safe temp workspace + EXIT trap (rollback + cleanup). Paths may contain
# spaces, everything is quoted.
# ---------------------------------------------------------------------------

TMP_WORK="$(mktemp -d "${TMPDIR:-/tmp}/ha-install.XXXXXXXX")"
TXN_DIRTY=0
BINARY_DIRTY=0
SKILL_DIRTY=0
SAVED_SKILL_DIR=""

on_exit() {
	rc=$?
	trap - EXIT
	if [ "$rc" -ne 0 ] && [ "$TXN_DIRTY" -eq 1 ]; then
		log "installation failed (exit $rc); rolling back"
		rollback
		log "rollback complete"
	fi
	rm -rf "$TMP_WORK"
	exit "$rc"
}
trap on_exit EXIT

# Rollback only undoes what the failed phase changed: a binary failure
# restores/replaces the binary, a skill failure restores the previous skill
# directory (saved on the same filesystem) and removes the partial one.
rollback() {
	if [ "$BINARY_DIRTY" -eq 1 ]; then
		if [ -f "$TMP_WORK/backup/ha" ]; then
			install -m 755 "$TMP_WORK/backup/ha" "$INSTALL_DIR/ha" ||
				log "warning: could not restore previous binary $INSTALL_DIR/ha"
		else
			rm -f "$INSTALL_DIR/ha" ||
				log "warning: could not remove partial binary $INSTALL_DIR/ha"
		fi
	fi
	if [ "$SKILL_DIRTY" -eq 1 ]; then
		if [ -n "$SAVED_SKILL_DIR" ] && [ -d "$SAVED_SKILL_DIR" ]; then
			rm -rf "$SKILLS_ROOT/ha-control" 2>/dev/null || true
			mv "$SAVED_SKILL_DIR" "$SKILLS_ROOT/ha-control" ||
				log "warning: could not restore previous skill from $SAVED_SKILL_DIR"
		elif [ -d "$SKILLS_ROOT/ha-control" ]; then
			rm -rf "$SKILLS_ROOT/ha-control" ||
				log "warning: could not remove partial skill at $SKILLS_ROOT/ha-control"
		fi
	fi
}

# Test hook (used by tests/installer); a no-op unless HA_CLI_TEST_FAIL_POINT
# matches the given point name.
failpoint() {
	if [ "${HA_CLI_TEST_FAIL_POINT:-}" = "${1:-}" ]; then
		die "failpoint reached: $1"
	fi
}

# ---------------------------------------------------------------------------
# Version helpers (kept together; tests extract this block for unit tests).
# --- version helpers ---

# semver_parse VER -> sets V_MAJ V_MIN V_PAT V_PRE (V_PRE empty = stable).
# Fails for anything that is not strict numeric x.y.z (optionally with a
# prerelease suffix). Leading zeros are safe: callers must use 10#$V.
semver_parse() {
	local v="$1" core
	[[ "$v" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || return 1
	core="${v%%-*}"
	case "$v" in
	*-*) V_PRE="${v#*-}" ;;
	*) V_PRE="" ;;
	esac
	V_MAJ="${core%%.*}"
	V_MIN="${core#*.}"
	V_MIN="${V_MIN%%.*}"
	V_PAT="${core##*.}"
	return 0
}

# version_ge A B — semver precedence: true if A >= B. A prerelease is less
# than the corresponding stable release (0.2.0-rc1 < 0.2.0).
version_ge() {
	local a_maj a_min a_pat a_pre b_maj b_min b_pat b_pre
	semver_parse "$1" || return 2
	a_maj=$V_MAJ a_min=$V_MIN a_pat=$V_PAT a_pre=$V_PRE
	semver_parse "$2" || return 2
	b_maj=$V_MAJ b_min=$V_MIN b_pat=$V_PAT b_pre=$V_PRE
	local c
	for c in "a_maj:b_maj" "a_min:b_min" "a_pat:b_pat"; do
		local av="${c%%:*}" bv="${c##*:}"
		if (( 10#${!av} > 10#${!bv} )); then return 0; fi
		if (( 10#${!av} < 10#${!bv} )); then return 1; fi
	done
	# Core equal: stable > prerelease; two prereleases compare lexically.
	if [ -z "$a_pre" ] && [ -n "$b_pre" ]; then return 0; fi
	if [ -n "$a_pre" ] && [ -z "$b_pre" ]; then return 1; fi
	if [ -z "$a_pre" ]; then return 0; fi
	[ "$a_pre" \> "$b_pre" ] || [ "$a_pre" = "$b_pre" ]
}

# --- end version helpers ---

installed_version() {
	# Prints e.g. 0.1.0 / 0.2.0-rc1 or "unknown".
	local v
	v="$("$1" --version 2>/dev/null | head -n 1 || true)"
	v="${v#ha-cli }"
	if semver_parse "$v"; then
		printf '%s' "$v"
	else
		printf 'unknown'
	fi
}

# target_triple — maps the host to a supported musl target or dies.
target_triple() {
	local os arch
	os="$(uname -s)" arch="$(uname -m)"
	case "$os" in
	Linux) ;;
	*)
		die "unsupported OS: $os (prebuilt binaries are Linux-only; rerun with --build-from-source)"
		;;
	esac
	case "$arch" in
	x86_64 | amd64) printf 'x86_64-unknown-linux-musl' ;;
	aarch64 | arm64) printf 'aarch64-unknown-linux-musl' ;;
	*)
		die "unsupported architecture: $arch (supported: x86_64, aarch64; rerun with --build-from-source)"
		;;
	esac
}

fetch_latest_tag() {
	local f
	f="$TMP_WORK/release.json"
	http_get "$API_URL" "$f"
	LATEST_TAG="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$f" | head -n 1)"
	[ -n "$LATEST_TAG" ] || die "could not determine latest release tag from $API_URL"
	[[ "$LATEST_TAG" == v* ]] && semver_parse "${LATEST_TAG#v}" ||
		die "invalid release tag: $LATEST_TAG"
	log "latest release: $LATEST_TAG"
}

# ---------------------------------------------------------------------------
# Download helpers (checksums, safe extraction).
# ---------------------------------------------------------------------------

http_get() {
	# http_get URL DEST
	curl -fsSL "$1" -o "$2" || die "download failed: $1"
}

# download_and_verify_tarball URL CHECKSUM_URL DEST_DIR
# Verifies the tarball against the published .sha256 (which references
# `dist/<name>`; only the hash field is used).
download_and_verify_tarball() {
	local url="$1" sum_url="$2" dest="$3" sum_file hash
	http_get "$url" "$dest/tarball.tar.gz"
	http_get "$sum_url" "$dest/tarball.tar.gz.sha256"
	sum_file="$dest/tarball.tar.gz.sha256"
	hash="$(awk 'NR==1 {print $1}' "$sum_file")"
	case "$hash" in
	[0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F]*) ;;
	*) die "invalid checksum file: $sum_url" ;;
	esac
	printf '%s  %s\n' "$hash" "$dest/tarball.tar.gz" | (cd "$dest" && sha256sum -c - >/dev/null) ||
		die "checksum mismatch for $url"
	log "checksum verified"
}

# safe_extract TARBALL DEST_DIR — refuses archives with absolute paths or
# `..` components, so extraction cannot escape DEST_DIR or overwrite
# arbitrary paths.
safe_extract() {
	local tarball="$1" dest="$2" bad
	if bad="$(tar -tzf "$tarball" | grep -E '(^|/)\.\.(/|$)|^/')" && [ -n "$bad" ]; then
		die "refusing to extract $tarball: unsafe path(s) in archive: $bad"
	fi
	tar -xzf "$tarball" --no-same-owner -C "$dest"
}

# ---------------------------------------------------------------------------
# Binary install.
# ---------------------------------------------------------------------------

install_binary_from_release() {
	local tag triple tar_name
	triple="$(target_triple)"
	tag="$LATEST_TAG"
	tar_name="ha-${tag}-${triple}.tar.gz"
	log "downloading $tar_name"
	download_and_verify_tarball \
		"${DL_BASE}/${tag}/${tar_name}${ASSET_URL_SUFFIX}" \
		"${DL_BASE}/${tag}/${tar_name}.sha256${ASSET_URL_SUFFIX}" \
		"$TMP_WORK/dl"
	safe_extract "$TMP_WORK/dl/tarball.tar.gz" "$TMP_WORK/dl"
	local new_bin
	new_bin="$TMP_WORK/dl/ha-${tag}-${triple}/ha"
	[ -f "$new_bin" ] || die "tarball does not contain ha-${tag}-${triple}/ha"

	install_binary "$new_bin"
}

install_binary() {
	local new_bin="$1"
	if [ -e "$INSTALL_DIR/ha" ]; then
		install -m 755 "$INSTALL_DIR/ha" "$TMP_WORK/backup/ha"
	fi
	# Stage then rename for an atomic-ish replace.
	install -m 755 "$new_bin" "$INSTALL_DIR/.ha.new"
	TXN_DIRTY=1
	BINARY_DIRTY=1
	mv "$INSTALL_DIR/.ha.new" "$INSTALL_DIR/ha"

	if ! "$INSTALL_DIR/ha" --help >/dev/null 2>&1; then
		die "installed binary failed smoke test"
	fi
	# Binary phase succeeded: the binary is committed, a later skill
	# failure must not undo it.
	BINARY_DIRTY=0
	TXN_DIRTY=0
	rm -f "$TMP_WORK/backup/ha"
	log "installed $INSTALL_DIR/ha ($(installed_version "$INSTALL_DIR/ha"))"
}

# ---------------------------------------------------------------------------
# Skill install.
# ---------------------------------------------------------------------------

# install_skill_dir SKILL_SRC_DIR — stages the skill into TMP_WORK (with the
# {{HA_BIN}} substitution) and swaps it into place. The previous skill
# directory is renamed aside on the same filesystem (kept until success) so
# rollback never has to cross a filesystem boundary and never destroys it.
install_skill_dir() {
	local skill_src="$1" dest="$SKILLS_ROOT/ha-control" ha_bin esc stage
	ha_bin="$INSTALL_DIR/ha"
	if [ ! -x "$ha_bin" ]; then
		log "warning: binary not found at $ha_bin (install it first with default mode or --install-dir)"
	fi

	stage="$TMP_WORK/ha-control"
	mkdir -p "$stage"
	# sed replacement escaping for \ & | (the delimiter is |).
	esc="${ha_bin//\\/\\\\}"
	esc="${esc//&/\\&}"
	esc="${esc//|/\\|}"
	sed -e "s|{{HA_BIN}}|$esc|g" "$skill_src/SKILL.md" >"$stage/SKILL.md"
	install -m 644 "$skill_src/SKILL.toml" "$stage/SKILL.toml"
	chmod 644 "$stage/SKILL.md"

	TXN_DIRTY=1
	SKILL_DIRTY=1
	if [ -d "$dest" ]; then
		SAVED_SKILL_DIR="$SKILLS_ROOT/.ha-control.bak.$$"
		mv "$dest" "$SAVED_SKILL_DIR"
	fi
	failpoint after_skill_backup
	if [ -e "$SAVED_SKILL_DIR" ]; then
		rm -rf "$dest" 2>/dev/null || true
	fi
	mv "$stage" "$dest"
	rm -rf "$SAVED_SKILL_DIR"
	SAVED_SKILL_DIR=""
	SKILL_DIRTY=0
	TXN_DIRTY=0
	log "installed OpenClaw skill into $dest"
}

install_skill_from_release() {
	local tag="$LATEST_TAG"
	log "downloading skill files for $tag"
	http_get "${SRC_BASE}/${tag}.tar.gz${ASSET_URL_SUFFIX}" "$TMP_WORK/src.tar.gz"
	safe_extract "$TMP_WORK/src.tar.gz" "$TMP_WORK"
	local skill_src
	skill_src="$(tar -tzf "$TMP_WORK/src.tar.gz" | sed -n 's#^\([^/]*/\)skills/ha-control/SKILL\.md$#\1#p' | head -n 1)"
	[ -n "$skill_src" ] || die "skills/ha-control not found in release source tarball"
	skill_src="$TMP_WORK/${skill_src%/}/skills/ha-control"
	[ -f "$skill_src/SKILL.toml" ] || die "SKILL.toml missing in $skill_src"

	install_skill_dir "$skill_src"
}

# ---------------------------------------------------------------------------
# Build from source (also supports --skills-root from the same checkout).
# ---------------------------------------------------------------------------

# confirm_source_build — asks before any network access / cargo invocation.
confirm_source_build() {
	if [ "$FORCE" -eq 1 ]; then
		return 0
	fi
	if [ ! -t 0 ]; then
		die "no TTY available for confirmation; rerun with --force to build without prompting"
	fi
	local answer
	printf 'install.sh: build from source (git clone if needed + cargo build --release)? [y/N] '
	read -r answer
	case "$answer" in
	y | Y | yes | Yes) ;;
	*) die "declined; aborting source build" ;;
	esac
}

build_from_source() {
	need_cmd cargo
	confirm_source_build

	# Prefer a nearby checkout (script run from a repo clone); only a local
	# lookup — no git required in this case.
	local src_dir=""
	if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
		local dir
		dir="$(cd -P "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
		if [ -f "$dir/Cargo.toml" ]; then
			src_dir="$dir"
		elif [ -f "$dir/../Cargo.toml" ]; then
			src_dir="$(cd -P "$dir/.." && pwd)"
		fi
	fi
	if [ -n "$src_dir" ]; then
		log "building from local checkout: $src_dir"
	else
		need_cmd git
		src_dir="$TMP_WORK/src"
		log "cloning repository into $src_dir"
		git clone -q --depth 1 "https://github.com/${REPO_SLUG}.git" "$src_dir" ||
			die "cannot clone repository"
	fi

	log "building release binary"
	cargo build --release --locked --manifest-path "$src_dir/Cargo.toml"
	local new_bin="$src_dir/target/release/ha"
	[ -x "$new_bin" ] || die "build did not produce $new_bin"
	install_binary "$new_bin"

	if [ -n "$SKILLS_ROOT" ]; then
		local skill_src="$src_dir/skills/ha-control"
		[ -f "$skill_src/SKILL.md" ] || die "SKILL.md not found in $skill_src"
		[ -f "$skill_src/SKILL.toml" ] || die "SKILL.toml missing in $skill_src"
		install_skill_dir "$skill_src"
	fi
}

# ---------------------------------------------------------------------------
# Main.
# ---------------------------------------------------------------------------

mkdir -p "$TMP_WORK/dl" "$TMP_WORK/backup"

if [ "$MODE" = "skill" ]; then
	fetch_latest_tag
	install_skill_from_release
	log "done"
	exit 0
fi

if [ "$BUILD_FROM_SOURCE" -eq 1 ]; then
	build_from_source
else
	fetch_latest_tag
	if [ -x "$INSTALL_DIR/ha" ]; then
		IV="$(installed_version "$INSTALL_DIR/ha")"
		LV="${LATEST_TAG#v}"
		if semver_parse "$LV"; then
			if [ "$IV" != "unknown" ] && version_ge "$IV" "$LV"; then
				log "installed ha $IV >= latest $LV; skipping binary install"
				if [ -n "$SKILLS_ROOT" ]; then
					install_skill_from_release
				else
					log "skipping OpenClaw skill (pass --skills-root PATH to install)"
				fi
				log "done"
				exit 0
			fi
			log "updating ha $IV -> $LV"
		else
			die "cannot parse version from release tag: $LATEST_TAG"
		fi
	fi
	install_binary_from_release
	if [ -n "$SKILLS_ROOT" ]; then
		install_skill_from_release
	else
		log "skipping OpenClaw skill (pass --skills-root PATH to install)"
	fi
fi

log "done"
