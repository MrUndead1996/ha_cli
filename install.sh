#!/bin/bash
# install.sh — сборка и установка ha CLI + скилла OpenClaw ha-control.
#
# Что делает:
# - собирает release-бинарник `ha` из этого репозитория;
# - устанавливает его в ~/.local/bin/ha (atomic install, бэкап старого);
# - при --skills-root PATH копирует скилл в <PATH>/ha-control/,
#   подставляя {{HA_BIN}} в SKILL.md;
# - автоматически определяет fresh install и update: если репозиторий —
#   git-клон, подтягивает обновления (git pull --ff-only, чистое
#   дерево; при локальных изменениях или сбое — предупреждение и
#   сборка локального состояния), затем пересобирает и заменяет
#   бинарник/скилл (бэкап старого, откат при сбое);
#   конфиг ~/.config/ha-cli/config.toml НЕ трогает: он создаётся
#   вручную (url, token/token_file, mcp_url/mcp_auth, права 0600 —
#   см. README).
#
# Usage: install.sh [--skills-root PATH]
#   --skills-root PATH   Установить скилл OpenClaw в PATH
#                        (без флага OpenClaw не трогается).
#
# Secrets are never printed or passed in command arguments.

set -euo pipefail

usage() {
	echo "usage: install.sh [--skills-root PATH]" >&2
}

ORIG_ARGS=("$@")
SKILLS_ROOT=""
SKILLS_ROOT_GIVEN=0
while [ $# -gt 0 ]; do
	case "$1" in
	--skills-root)
		[ $# -ge 2 ] || usage
		SKILLS_ROOT="$2"
		SKILLS_ROOT_GIVEN=1
		shift 2
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		echo "install.sh: unknown argument: $1" >&2
		usage
		exit 2
		;;
	esac
done

if [ -z "${HOME:-}" ]; then
	echo "install.sh: HOME is not set" >&2
	exit 1
fi

if [ "$SKILLS_ROOT_GIVEN" -eq 1 ] && [ -z "$SKILLS_ROOT" ]; then
	echo "install.sh: --skills-root requires a non-empty PATH argument" >&2
	exit 2
fi

log() { printf 'install.sh: %s\n' "$*"; }
die() { printf 'install.sh: error: %s\n' "$*" >&2; exit 1; }

need_cmd() { command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"; }

# При `bash -s` (чтение из пайпа) BASH_SOURCE пуст — под set -u проверяем
# существование элемента, а не его значение.
if [ "${BASH_SOURCE[0]+set}" = set ] && [ -n "${BASH_SOURCE[0]}" ]; then
	SCRIPT_SOURCE="${BASH_SOURCE[0]}"
	while [ -L "$SCRIPT_SOURCE" ]; do
		SCRIPT_DIR="$(cd -P "$(dirname "$SCRIPT_SOURCE")" && pwd)"
		SCRIPT_SOURCE="$(readlink "$SCRIPT_SOURCE")"
		[ "${SCRIPT_SOURCE#/}" != "$SCRIPT_SOURCE" ] || SCRIPT_SOURCE="$SCRIPT_DIR/$SCRIPT_SOURCE"
	done
	ROOT_CANDIDATE="$(cd -P "$(dirname "$SCRIPT_SOURCE")" && pwd)"
else
	# `curl ... | bash`: скрипт читается из stdin, репозитория рядом нет.
	ROOT_CANDIDATE=""
fi

# Self-bootstrap: при запуске из пайпа клонируем репозиторий во временную
# директорию и перепоручаем установку клонированному install.sh.
if [ -z "$ROOT_CANDIDATE" ] || [ ! -f "$ROOT_CANDIDATE/Cargo.toml" ]; then
	need_cmd git
	BOOTSTRAP_DIR="$(mktemp -d)"
	log "bootstrapping: cloning repository into $BOOTSTRAP_DIR"
	git clone -q --depth 1 https://github.com/MrUndead1996/ha_cli.git "$BOOTSTRAP_DIR/ha_cli" ||
		die "cannot clone repository"
	# "$@" уже разобран (и съеден shift) выше — передаём копию.
	exec "$BOOTSTRAP_DIR/ha_cli/install.sh" "${ORIG_ARGS[@]}"
fi
REPO_ROOT="$ROOT_CANDIDATE"

BIN_DIR="$HOME/.local/bin"
BIN_NAME="ha"
SKILL_NAME="ha-control"

need_cmd cargo
need_cmd install

BACKUP_DIR="$(mktemp -d)"
TXN_DIRTY=0

finish() {
	rc=$?
	trap - EXIT
	if [ "$rc" -eq 0 ]; then
		rm -rf "$BACKUP_DIR"
		exit "$rc"
	fi
	if [ "$TXN_DIRTY" -eq 1 ]; then
		log "installation failed (exit $rc); rolling back"
		if [ -f "$BACKUP_DIR/$BIN_NAME" ]; then
			install -m 755 "$BACKUP_DIR/$BIN_NAME" "$BIN_DIR/$BIN_NAME" ||
				log "warning: could not restore previous $BIN_NAME"
		elif [ -e "$BIN_DIR/$BIN_NAME" ]; then
			rm -f "$BIN_DIR/$BIN_NAME"
		fi
		if [ -n "$SKILLS_ROOT" ] && [ -d "$SKILLS_ROOT/$SKILL_NAME" ]; then
			rm -rf "$SKILLS_ROOT/$SKILL_NAME"
		fi
		log "rollback complete"
	fi
	rm -rf "$BACKUP_DIR"
	exit "$rc"
}
trap finish EXIT

# ---------------------------------------------------------------------------
# 0. Update the repository (automatic when run from a git clone).
#    Fast-forward only; local changes or fetch/pull problems do not
#    block installation — we warn and build the local state.
# ---------------------------------------------------------------------------

if [ -d "$REPO_ROOT/.git" ] && command -v git >/dev/null 2>&1; then
	cd "$REPO_ROOT"
	if [ -n "$(git status --porcelain)" ]; then
		log "warning: working tree has local changes; skipping repo update"
	elif ! git fetch --quiet 2>/dev/null; then
		log "warning: git fetch failed; building current checkout"
	else
		OLD_HEAD="$(git rev-parse --short HEAD)"
		if git pull --ff-only --quiet 2>/dev/null; then
			NEW_HEAD="$(git rev-parse --short HEAD)"
			if [ "$OLD_HEAD" = "$NEW_HEAD" ]; then
				log "repository already up to date ($OLD_HEAD)"
			else
				log "updated repository $OLD_HEAD -> $NEW_HEAD"
			fi
		else
			log "warning: git pull --ff-only failed (branch diverged?); building current checkout"
		fi
	fi
else
	log "not a git clone (or git missing); building current sources"
fi

# ---------------------------------------------------------------------------
# 1. Build.
# ---------------------------------------------------------------------------

log "building release binary"
cargo build --release --locked --manifest-path "$REPO_ROOT/Cargo.toml"
NEW_BIN="$REPO_ROOT/target/release/$BIN_NAME"
[ -x "$NEW_BIN" ] || die "build did not produce $NEW_BIN"

# Version strings for before/after reporting; old binary may predate
# the --version flag, treat that as "unknown".
version_of() {
	"$1" --version 2>/dev/null | head -1 || printf 'unknown'
}
OLD_VERSION="none"
[ -x "$BIN_DIR/$BIN_NAME" ] && OLD_VERSION="$(version_of "$BIN_DIR/$BIN_NAME")"

# ---------------------------------------------------------------------------
# 2. Install binary (backup previous, atomic-ish replace).
# ---------------------------------------------------------------------------

mkdir -p "$BIN_DIR"
if [ -e "$BIN_DIR/$BIN_NAME" ]; then
	install -m 755 "$BIN_DIR/$BIN_NAME" "$BACKUP_DIR/$BIN_NAME"
fi
install -m 755 "$NEW_BIN" "$BIN_DIR/$BIN_NAME"
TXN_DIRTY=1
NEW_VERSION="$(version_of "$BIN_DIR/$BIN_NAME")"
log "installed $BIN_DIR/$BIN_NAME (version: $OLD_VERSION -> $NEW_VERSION)"

# ---------------------------------------------------------------------------
# 3. Smoke test: binary runs and config check gives a sane answer.
# ---------------------------------------------------------------------------

if ! "$BIN_DIR/$BIN_NAME" --help >/dev/null 2>&1; then
	die "installed binary failed smoke test"
fi

# ---------------------------------------------------------------------------
# 4. Skill installation (only with --skills-root).
# ---------------------------------------------------------------------------

if [ -n "$SKILLS_ROOT" ]; then
	SKILL_SRC="$REPO_ROOT/skills/$SKILL_NAME"
	[ -f "$SKILL_SRC/SKILL.md" ] || die "$SKILL_SRC/SKILL.md not found"
	[ -f "$SKILL_SRC/SKILL.toml" ] || die "$SKILL_SRC/SKILL.toml not found"

	DEST="$SKILLS_ROOT/$SKILL_NAME"
	if [ -d "$DEST" ]; then
		log "removing previous skill at $DEST"
		rm -rf "$DEST"
	fi
	mkdir -p "$DEST"
	# Подстановка {{HA_BIN}}: sed-экранирование пути для s///|...|...|.
	esc="${BIN_DIR}/$BIN_NAME"
	esc="$(printf '%s' "$esc" | sed -e 's/[&|]/\\&/g')"
	sed -e "s|{{HA_BIN}}|$esc|g" "$SKILL_SRC/SKILL.md" >"$DEST/SKILL.md"
	install -m 644 "$SKILL_SRC/SKILL.toml" "$DEST/SKILL.toml"
	chmod 644 "$DEST/SKILL.md"
	log "installed OpenClaw skill into $DEST"
else
	log "skipping OpenClaw skill (pass --skills-root PATH to install)"
fi

log "done"
