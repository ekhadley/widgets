PREFIX ?= /usr

install:
	cargo build --release -p wallrun
	install -Dm755 ../target/release/wallrun $(PREFIX)/bin/wallrun
