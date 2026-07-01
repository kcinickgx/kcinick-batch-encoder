# Makefile — builds the Linux TUI client (fast6t).
# (For the Windows GUI/daemon use build.bat.)
#
# The GUI (fast6, eframe) and the daemon (fast6d, eframe + tray-icon) don't link on
# Linux without system libraries (GTK3, libxdo, etc.). So the Linux deliverable is the
# TUI client. `make` builds fast6t in release and copies it to the repo root.

CARGO  ?= cargo
PREFIX ?= $(HOME)/.local
BIN    := fast6t
REL    := target/release/$(BIN)

.PHONY: all build release debug run install uninstall daemon clean help

all: release

build: release

release:
	$(CARGO) build --release -p fast6tui
	cp -f $(REL) ./$(BIN)
	@echo
	@echo "Done:"
	@echo "  ./$(BIN)   (Linux TUI client)"

debug:
	$(CARGO) build -p fast6tui

run: release
	./$(BIN)

install: release
	install -Dm755 $(REL) $(DESTDIR)$(PREFIX)/bin/$(BIN)
	@echo "Installed: $(DESTDIR)$(PREFIX)/bin/$(BIN)"

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/$(BIN)

# Optional daemon: requires GTK3 + libxdo to link on Linux.
daemon:
	$(CARGO) build --release -p fast6d

clean:
	$(CARGO) clean
	rm -f ./$(BIN)

help:
	@echo "make            build the TUI in release and copy ./$(BIN)"
	@echo "make debug      debug build (target/debug/$(BIN))"
	@echo "make run        build + run ./$(BIN)"
	@echo "make install    install into \$$PREFIX/bin (default: ~/.local/bin)"
	@echo "make uninstall  remove the installed binary"
	@echo "make daemon     build fast6d (requires GTK3 + libxdo)"
	@echo "make clean      cargo clean + rm ./$(BIN)"
