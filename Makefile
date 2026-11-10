.PHONY: install update open uninstall run test editor

install:
	./scripts/install.sh

update: install

open:
	./scripts/install.sh --open

uninstall:
	./scripts/install.sh --uninstall

run:
	cargo run -- .

test:
	cargo test

editor:
	cd editor-js && npm install --silent && npm run build
