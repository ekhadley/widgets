PREFIX ?= $(HOME)/.local
WIDGETS = wallrun grimoire wavedash

install:
ifdef W
	cargo build --release -p $(W)
	install -Dm755 target/release/$(W) $(PREFIX)/bin/$(W)
	$(if $(filter wavedash,$(W)),printf '#!/bin/sh\npkill -x wavedash || wavedash &\n' > $(PREFIX)/bin/wavedash_toggle && chmod 755 $(PREFIX)/bin/wavedash_toggle)
else
	cargo build --release
	$(foreach w,$(WIDGETS),install -Dm755 target/release/$(w) $(PREFIX)/bin/$(w);)
	printf '#!/bin/sh\npkill -x wavedash || wavedash &\n' > $(PREFIX)/bin/wavedash_toggle
	chmod 755 $(PREFIX)/bin/wavedash_toggle
endif
