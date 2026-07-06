# ctxgrd — build and install.
#
#   make install                     # install to ~/.local/bin/ctxgrd
#   make install PREFIX=/usr/local   # install to /usr/local/bin/ctxgrd  (needs sudo)
#   make uninstall                   # remove from $(BINDIR)
#   make test | make check | make fmt | make clean
#
# PREFIX and BINDIR follow the GNU convention — override either on the
# command line. TARGET picks release vs debug (release is the default).

PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
CARGO  ?= cargo
TARGET ?= release

ifeq ($(TARGET),release)
BUILD_FLAGS := --release
else
BUILD_FLAGS :=
endif

# Resolve cargo's target directory — honors CARGO_TARGET_DIR and the
# `target-dir` setting in .cargo/config.toml; falls back to ./target.
TARGET_DIR := $(shell $(CARGO) metadata --no-deps --format-version 1 2>/dev/null | grep -o '"target_directory":"[^"]*"' | head -1 | cut -d'"' -f4)
ifeq ($(TARGET_DIR),)
TARGET_DIR := target
endif

BIN := $(TARGET_DIR)/$(TARGET)/ctxgrd

.PHONY: help build install install-debug uninstall test ci check lint fmt clean run-example

help:
	@echo "ctxgrd — Makefile targets"
	@echo
	@echo "  build        Build the release binary ($(BIN))"
	@echo "  install      Install ctxgrd to $(BINDIR)/"
	@echo "  install-debug Install the unoptimized debug build (fast to compile)"
	@echo "  uninstall    Remove ctxgrd from $(BINDIR)/"
	@echo "  test         Run all tests"
	@echo "  ci           check + test — the canonical gate"
	@echo "  check        cargo check + cargo clippy -D warnings"
	@echo "  fmt          Format every file with rustfmt"
	@echo "  clean        cargo clean"
	@echo "  run-example  Build and lint examples/"
	@echo
	@echo "Variables:"
	@echo "  PREFIX=$(PREFIX)"
	@echo "  BINDIR=$(BINDIR)"
	@echo "  TARGET=$(TARGET)  (release|debug)"

build:
	$(CARGO) build $(BUILD_FLAGS)

install: build
	install -d $(BINDIR)
	install -m 0755 $(BIN) $(BINDIR)/ctxgrd
	@echo
	@echo "  Installed $(BIN) → $(BINDIR)/ctxgrd"
	@case ":$$PATH:" in \
	  *":$(BINDIR):"*) ;; \
	  *) echo "  Note: $(BINDIR) is NOT on \$$PATH. Add it to your shell rc, e.g.:"; \
	     echo "    echo 'export PATH=\"$(BINDIR):\$$PATH\"' >> ~/.zshrc" ;; \
	esac

# Install the debug build — skips the fat-LTO/codegen-units=1 release
# profile, so it compiles fast. Slower at runtime; use for local iteration.
install-debug:
	$(MAKE) install TARGET=debug

uninstall:
	rm -f $(BINDIR)/ctxgrd

test:
	$(CARGO) test

# Canonical gate: the full lint+lib check plus the whole test suite
# (unit + every integration suite, including the SPEC-002 acceptance
# scenarios in tests/status.rs). Point CI at this single target.
ci: check test

check: adr-lint changelog-check
	$(CARGO) check --all-targets
	$(CARGO) clippy --lib --no-deps -- -D warnings

# Self-lint the project's own ADRs. Uses the installed `ctxgrd` binary
# from $$PATH (run `make install` once before this works). Falls back
# to building + running the local debug binary if not on PATH.
adr-lint:
	@if command -v ctxgrd > /dev/null 2>&1; then \
	  ctxgrd; \
	else \
	  echo "ctxgrd not on PATH — using local debug build"; \
	  $(CARGO) run --quiet -- ; \
	fi

# Gate CHANGELOG.md freshness (ADR-084 § CHG-001/CHG-005): the committed
# changelog must match what `ctxgrd changelog --write` would regenerate.
# Same install-or-local-debug fallback as adr-lint.
changelog-check:
	@if command -v ctxgrd > /dev/null 2>&1; then \
	  ctxgrd changelog --check; \
	else \
	  echo "ctxgrd not on PATH — using local debug build"; \
	  $(CARGO) run --quiet -- changelog --check; \
	fi

lint: check

fmt:
	$(CARGO) fmt --all

clean:
	$(CARGO) clean

run-example: build
	$(BIN) --root examples
