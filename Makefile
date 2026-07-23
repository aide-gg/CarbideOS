# SPDX-License-Identifier: AGPL-3.0-or-later

.PHONY: keys production-keys production-sysext-cert build debug fleet extensions fleet-extensions package fleet-package sign fleet-sign verify pipeline fleet-pipeline publish-r2 publish-fleet clean clean-tools

keys:
	./scripts/provision-keys

production-keys:
	./scripts/provision-production-keys

production-sysext-cert:
	./scripts/reissue-production-sysext-certificate

build:
	./scripts/build

debug:
	./scripts/build --debug

fleet:
	./scripts/build --fleet

extensions:
	./extensions/rat-game-16/build

fleet-extensions:
	./extensions/rat-game-16/build --fleet

package:
	./scripts/package

fleet-package:
	./scripts/package --fleet

sign:
	./scripts/sign

fleet-sign:
	./scripts/sign --fleet

verify:
	./scripts/verify

pipeline: build extensions package sign verify

fleet-pipeline:
	./scripts/fleet-pipeline

publish-fleet:
	CARBIDEOS_R2_PREFIX=carbideos/fleet \
	CARBIDEOS_RELEASE_GNUPGHOME=$(CURDIR)/keys/production/manifest/gnupg \
	./scripts/publish-r2 dist/update-feed/fleet

publish-r2:
	@test -n "$(SOURCE)" || { echo 'Usage: make publish-r2 SOURCE=dist/update-feed' >&2; exit 2; }
	./scripts/publish-r2 "$(SOURCE)"

clean:
	sudo find mkosi.output -maxdepth 1 \( -type f -o -type l \) -name 'carbideos*' -delete 2>/dev/null || true
	rm -rf dist

clean-tools:
	sudo mkosi -f clean
