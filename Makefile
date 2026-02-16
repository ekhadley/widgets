PREFIX ?= /usr

install:
	cargo build --release
	install -Dm755 target/release/wallrun $(PREFIX)/bin/wallrun
	install -Dm755 target/release/grimoire $(PREFIX)/bin/grimoire
	install -Dm755 target/release/raven $(PREFIX)/bin/raven
	printf '#!/bin/sh\npkill -x raven || raven &\n' > $(PREFIX)/bin/raven_toggle
	chmod 755 $(PREFIX)/bin/raven_toggle
