SANDSTORM_SPK ?= $(HOME)/src/sandstorm/build/dev/bin/spk
CONTACT_EMAIL ?= maintainer@example.invalid

.PHONY: build package test test-package test-interop
build:
	cargo build --release -p marketplace-grain -p marketplace-web

package:
	cargo xtask package --contact-email "$(CONTACT_EMAIL)"

test:
	cargo test --workspace
	node --test tests/frontend.cjs

test-interop:
	cargo xtask test-interop --spk "$(SANDSTORM_SPK)"

test-package:
	cargo xtask test-package
