PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
BINARY := agent-console
BUILD_BINARY := target/release/$(BINARY)

.PHONY: install
install:
	cargo build --release
	mkdir -p "$(BINDIR)"
	install -m 755 "$(BUILD_BINARY)" "$(BINDIR)/$(BINARY).new"
	mv -f "$(BINDIR)/$(BINARY).new" "$(BINDIR)/$(BINARY)"
	@"$(BINDIR)/$(BINARY)" --version
