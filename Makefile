PREFIX ?= $(HOME)/.local
WIDGETS = wallrun grimoire raven

install:
ifdef W
	cargo build --release -p $(W)
	install -Dm755 target/release/$(W) $(PREFIX)/bin/$(W)
	$(if $(filter raven,$(W)),printf '#!/bin/sh\npkill -x raven || raven &\n' > $(PREFIX)/bin/raven_toggle && chmod 755 $(PREFIX)/bin/raven_toggle)
else
	cargo build --release
	$(foreach w,$(WIDGETS),install -Dm755 target/release/$(w) $(PREFIX)/bin/$(w);)
	printf '#!/bin/sh\npkill -x raven || raven &\n' > $(PREFIX)/bin/raven_toggle
	chmod 755 $(PREFIX)/bin/raven_toggle
endif
