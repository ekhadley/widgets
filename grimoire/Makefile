PREFIX ?= $(HOME)/.local

install:
	cargo build --release
	install -Dm755 target/release/grimoire $(PREFIX)/bin/grimoire
