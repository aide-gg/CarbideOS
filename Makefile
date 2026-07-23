# SPDX-License-Identifier: AGPL-3.0-or-later

.PHONY: keys production-keys production-sysext-cert playground-keys build debug fleet playground extensions fleet-extensions playground-extensions package fleet-package playground-package sign fleet-sign playground-sign verify pipeline fleet-pipeline playground-pipeline publish-r2 publish-fleet publish-playground clean clean-tools

keys:
	./scripts/provision-keys

production-keys:
	./scripts/provision-production-keys

production-sysext-cert:
	./scripts/reissue-production-sysext-certificate

playground-keys:
	./scripts/provision-playground-keys

build:
	./scripts/build

debug:
	./scripts/build --debug

fleet:
	./scripts/build --fleet

playground:
	./scripts/build --playground

extensions:
	./extensions/rat-game-16/build

fleet-extensions:
	./extensions/rat-game-16/build --fleet

playground-extensions:
	./extensions/rat-game-16/build --playground

package:
	./scripts/package

fleet-package:
	./scripts/package --fleet

playground-package:
	./scripts/package --playground

sign:
	./scripts/sign

fleet-sign:
	./scripts/sign --fleet

playground-sign:
	./scripts/sign --playground

verify:
	./scripts/verify

pipeline: build extensions package sign verify

fleet-pipeline:
	./scripts/fleet-pipeline

playground-pipeline:
	./scripts/playground-pipeline

publish-fleet:
	CARBIDEOS_R2_PREFIX=carbideos/fleet \
	./scripts/publish-r2 dist/update-feed/fleet

publish-playground:
	CARBIDEOS_R2_PREFIX=carbideos/playground \
	./scripts/publish-r2 dist/update-feed/playground

publish-r2:
	@test -n "$(SOURCE)" || { echo 'Usage: make publish-r2 SOURCE=dist/update-feed' >&2; exit 2; }
	./scripts/publish-r2 "$(SOURCE)"

clean:
	sudo find mkosi.output -maxdepth 1 \( -type f -o -type l \) -name 'carbideos*' -delete 2>/dev/null || true
	rm -rf dist

clean-tools:
	sudo mkosi -f clean
