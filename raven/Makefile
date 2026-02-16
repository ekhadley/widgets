PREFIX ?= /usr

install:
	cargo build --release -p raven
	install -Dm755 ../target/release/raven $(PREFIX)/bin/raven
	printf '#!/bin/sh\npkill -x raven || raven &\n' > $(PREFIX)/bin/raven_toggle
	chmod 755 $(PREFIX)/bin/raven_toggle
