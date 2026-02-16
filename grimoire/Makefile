PREFIX ?= /usr

install:
	cargo build --release -p grimoire
	install -Dm755 ../target/release/grimoire $(PREFIX)/bin/grimoire
