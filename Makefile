# cargo has no post-build hook, so code signing lives here.
# See scripts/codesign.sh for why dev builds are signed at all.

.PHONY: build build-streaming release release-streaming install uninstall run daemon tui test lint fmt check setup-codesign unsign-teardown

build:
	cargo build
	@./scripts/codesign.sh target/debug/boombox
	@echo "note: no streaming or visualisations. use 'make build-streaming' for those."

# NOTE: target/debug/boombox is shared between feature sets -- a plain `make
# build` overwrites the streaming binary and vice versa.
build-streaming:
	cargo build --features streaming
	@./scripts/codesign.sh target/debug/boombox

release:
	cargo build --release
	@./scripts/codesign.sh target/release/boombox

release-streaming:
	cargo build --release --features streaming
	@./scripts/codesign.sh target/release/boombox

# Puts boombox on your PATH with everything switched on. Without this, a bare
# `boombox` runs whatever cargo installed last, which is not what you just built.
install:
	cargo install --locked --path crates/boombox --features streaming --force
	@./scripts/codesign.sh "$$HOME/.cargo/bin/boombox"
	@echo "installed $$("$$HOME/.cargo/bin/boombox" --version)"
	@echo "          $$HOME/.cargo/bin/boombox"
# Another boombox earlier on PATH would shadow the one just installed, and
# every later command would quietly be the old one.
	@found=$$(command -v boombox 2>/dev/null || true); \
	  if [ -n "$$found" ] && [ "$$found" != "$$HOME/.cargo/bin/boombox" ]; then \
	    echo "warning:  PATH finds $$found first"; \
	  fi
# A daemon keeps the build it started with, however many times this runs.
	@if "$$HOME/.cargo/bin/boombox" daemon --status >/dev/null 2>&1; then \
	  running=$$("$$HOME/.cargo/bin/boombox" daemon --status | awk '/^version/ {$$1=""; print substr($$0, 2)}'); \
	  echo "note:     the running daemon is still $$running"; \
	  echo "          \`boombox daemon --stop\` replaces it"; \
	fi

# `make install` is cargo underneath, so this is too. It says what it has
# left behind: config and tokens outliving an uninstall is deliberate --
# reinstalling should not mean signing in again -- but it is worth being
# told rather than discovering later.
uninstall:
	@cargo uninstall boombox || echo "nothing installed by cargo to remove"
	@echo
	@echo "left alone, so a reinstall does not mean signing in again:"
	@echo "  $$HOME/.config/boombox"
	@echo "  $$HOME/.local/state/boombox"
	@echo "remove those for a clean slate; 'make unsign-teardown' removes the"
	@echo "signing certificate, if you ever ran 'make setup-codesign'."

run: build
	./target/debug/boombox

tui: build
	./target/debug/boombox tui

daemon: build
	./target/debug/boombox daemon

test:
	cargo test
	cargo test --features streaming

lint:
	cargo clippy --all-targets

fmt:
	cargo fmt

check: fmt lint test

# One-time: create the local signing identity (asks for your login password).
setup-codesign:
	./scripts/setup-codesign.sh

# Remove the signing identity, its trust setting, and the search-list entry.
unsign-teardown:
	-security list-keychains -d user -s "$$HOME/Library/Keychains/login.keychain-db"
	-security remove-trusted-cert "$$HOME/Library/Keychains/boombox-codesign.crt"
	-security delete-keychain "$$HOME/Library/Keychains/boombox-codesign.keychain-db"
	-rm -f "$$HOME/Library/Keychains/boombox-codesign.crt"
	@echo "signing identity removed; builds go back to ad-hoc signatures"
