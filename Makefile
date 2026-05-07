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

BIN := target/$(TARGET)/ctxgrd

.PHONY: help build install uninstall test check lint fmt clean run-example

help:
	@echo "ctxgrd — Makefile targets"
	@echo
	@echo "  build        Build the release binary ($(BIN))"
	@echo "  install      Install ctxgrd to $(BINDIR)/"
	@echo "  uninstall    Remove ctxgrd from $(BINDIR)/"
	@echo "  test         Run all tests"
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

uninstall:
	rm -f $(BINDIR)/ctxgrd

test:
	$(CARGO) test

check: adr-lint
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

lint: check

fmt:
	$(CARGO) fmt --all

clean:
	$(CARGO) clean

run-example: build
	$(BIN) --root examples
