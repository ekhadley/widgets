PREFIX ?= $(HOME)/.local

install:
	cargo build --release
	install -Dm755 target/release/stele $(PREFIX)/bin/stele
	printf '#!/bin/sh\npkill -x stele || stele &\n' > $(PREFIX)/bin/stele_toggle
	chmod 755 $(PREFIX)/bin/stele_toggle
