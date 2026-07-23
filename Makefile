# SPDX-License-Identifier: AGPL-3.0-or-later

.PHONY: keys build debug package sign verify pipeline publish-r2 clean clean-tools

keys:
	./scripts/provision-keys

build:
	./scripts/build

debug:
	./scripts/build --debug

package:
	./scripts/package

sign:
	./scripts/sign

verify:
	./scripts/verify

pipeline: build package sign verify

publish-r2:
	@test -n "$(SOURCE)" || { echo 'Usage: make publish-r2 SOURCE=dist/update-feed' >&2; exit 2; }
	./scripts/publish-r2 "$(SOURCE)"

clean:
	sudo find mkosi.output -maxdepth 1 \( -type f -o -type l \) -name 'carbideos*' -delete 2>/dev/null || true
	rm -rf dist

clean-tools:
	sudo mkosi -f clean
