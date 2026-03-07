PREFIX ?= $(HOME)/.local
WIDGETS = wallrun grimoire wavedash evoke

install:
ifdef W
	cargo build --release -p $(W)
	install -Dm755 target/release/$(W) $(PREFIX)/bin/$(W)
	$(if $(filter wavedash,$(W)),printf '#!/bin/sh\npkill -x wavedash || wavedash &\n' > $(PREFIX)/bin/wavedash_toggle && chmod 755 $(PREFIX)/bin/wavedash_toggle)
	$(if $(filter wallrun,$(W)),printf '#!/bin/sh\npgrep -x wallrun && exit 0\nwallrun "$$@"\n' > $(PREFIX)/bin/wallrun_toggle && chmod 755 $(PREFIX)/bin/wallrun_toggle)
	$(if $(filter grimoire,$(W)),printf '#!/bin/sh\npgrep -x grimoire && exit 0\ngrimoire "$$@"\n' > $(PREFIX)/bin/grimoire_toggle && chmod 755 $(PREFIX)/bin/grimoire_toggle)
	$(if $(filter evoke,$(W)),printf '#!/bin/sh\npkill -x -USR1 evoke || evoke &\n' > $(PREFIX)/bin/evoke_toggle && chmod 755 $(PREFIX)/bin/evoke_toggle)
else
	cargo build --release
	$(foreach w,$(WIDGETS),install -Dm755 target/release/$(w) $(PREFIX)/bin/$(w);)
	printf '#!/bin/sh\npkill -x wavedash || wavedash &\n' > $(PREFIX)/bin/wavedash_toggle
	chmod 755 $(PREFIX)/bin/wavedash_toggle
	printf '#!/bin/sh\npgrep -x wallrun && exit 0\nwallrun "$$@"\n' > $(PREFIX)/bin/wallrun_toggle
	chmod 755 $(PREFIX)/bin/wallrun_toggle
	printf '#!/bin/sh\npgrep -x grimoire && exit 0\ngrimoire "$$@"\n' > $(PREFIX)/bin/grimoire_toggle
	chmod 755 $(PREFIX)/bin/grimoire_toggle
	printf '#!/bin/sh\npkill -x -USR1 evoke || evoke &\n' > $(PREFIX)/bin/evoke_toggle
	chmod 755 $(PREFIX)/bin/evoke_toggle
endif
