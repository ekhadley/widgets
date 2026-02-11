PREFIX ?= $(HOME)/.local

install:
	cargo build --release
	install -Dm755 target/release/panel $(PREFIX)/bin/panel
	printf '#!/bin/sh\npkill -x panel || panel &\n' > $(PREFIX)/bin/panel_toggle
	chmod 755 $(PREFIX)/bin/panel_toggle
